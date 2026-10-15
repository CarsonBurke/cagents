//! Durable negative custody for explicitly identified project operations.
//!
//! The cumulative future migration controller owns this table. A row never
//! grants directory, provider, or close authority. After restart an unfinished
//! row only preserves refusal: its original handles and FDs cannot be recreated
//! from these paths. The live reservation owns the actual prepared phase.
use super::*;
use crate::model::{CardId, TerminalId};
use crate::project_recovery::{ProjectInstance, Relocation};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_PENDING: usize = crate::project_recovery::PROJECT_OPERATION_PENDING_LIMIT;
pub(crate) const MAX_OPERATIONS: usize = 65_536;
const MAX_SCOPE_BYTES: usize = 1024 * 1024;

/// Only negative scope metadata is durable. No descriptor, factory, environment,
/// or process ownership is reconstructed from it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedScope {
    pub project: ProjectInstance,
    pub relocation: Option<Relocation>,
    pub roots: Vec<PathBuf>,
    pub protected: Vec<PathBuf>,
    pub channels: Vec<ChannelId>,
    pub panes: Vec<CardId>,
    pub sessions: Vec<TerminalId>,
}
impl SavedScope {
    fn validate(&self) -> Result<(), String> {
        self.project.validate()?;
        if let Some(relocation) = &self.relocation {
            relocation.validate()?;
            if relocation.repository != self.project.repository {
                return Err("Project operation scope does not name its original project".into());
            }
        }
        if self.project.repository.as_os_str().as_encoded_bytes().len()>16384
            || self.roots.iter().chain(&self.protected).any(|path|path.as_os_str().as_encoded_bytes().len()>16384)
            || self.roots.len() > 8192 || self.protected.len() > 8192
            || self.channels.len() > 8192 || self.panes.len() > 8192
            || self.sessions.len() > 8192
            || self.roots.iter().chain(&self.protected).any(|path| !path.is_absolute()
                || path.components().any(|part| matches!(part, std::path::Component::ParentDir)))
        {
            return Err("Project operation negative scope is invalid or too large".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Admitted,
    Repairing,
    Closing,
    PendingCommit,
    ModelCommitted,
    Resolved,
    Aborted,
}
impl Phase {
    fn text(self) -> &'static str {
        match self {
            Self::Admitted => "admitted", Self::Repairing => "repairing",
            Self::Closing => "closing", Self::PendingCommit => "pending_commit",
            Self::ModelCommitted => "model_committed", Self::Resolved => "resolved",
            Self::Aborted => "aborted",
        }
    }
    fn parse(text: &str) -> Result<Self, String> {
        Ok(match text {
            "admitted" => Self::Admitted, "repairing" => Self::Repairing,
            "closing" => Self::Closing, "pending_commit" => Self::PendingCommit,
            "model_committed" => Self::ModelCommitted, "resolved" => Self::Resolved,
            "aborted" => Self::Aborted,
            _ => return Err("Unknown project operation custody phase".into()),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Obligation {
    pub operation: Uuid,
    pub project: ProjectInstance,
    pub request_digest: String,
    pub commit_digest: String,
    pub phase: Phase,
    pub scope: Option<SavedScope>,
    pub committed_sequence: Option<u64>,
}

// EXACT future-controller DDL leaf; no user_version or selected18 mutation.
// The sole outer controller installs these exact objects in its crash-safe
// migration transaction. This runtime module exposes no migration entry point.
pub(crate) const CREATE_TABLE: &str = "CREATE TABLE project_operation_obligations (
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
) STRICT;
CREATE INDEX project_operation_obligations_pending_project
 ON project_operation_obligations(registry,repository,project_ordinal)
 WHERE phase NOT IN ('resolved','aborted');";

struct BoundedWriter<W> {inner:W,remaining:usize}
impl<W:io::Write> io::Write for BoundedWriter<W> {
    fn write(&mut self,bytes:&[u8])->io::Result<usize> {
        if bytes.len()>self.remaining {return Err(io::Error::other("Project operation serialization exceeds its custody bound"));}
        self.inner.write_all(bytes)?;self.remaining-=bytes.len();Ok(bytes.len())
    }
    fn flush(&mut self)->io::Result<()> {self.inner.flush()}
}
fn bounded_scope(scope:&SavedScope)->Result<String,String> {
    let mut writer=BoundedWriter{inner:Vec::new(),remaining:MAX_SCOPE_BYTES};
    serde_json::to_writer(&mut writer,scope).map_err(|error|error.to_string())?;
    String::from_utf8(writer.inner).map_err(|error|error.to_string())
}
pub(crate) fn digest(value:&impl Serialize)->Result<String,String> {
    use sha2::Digest;
    struct HashWriter(sha2::Sha256);
    impl io::Write for HashWriter {
        fn write(&mut self,bytes:&[u8])->io::Result<usize> {self.0.update(bytes);Ok(bytes.len())}
        fn flush(&mut self)->io::Result<()> {Ok(())}
    }
    let mut writer=BoundedWriter{inner:HashWriter(sha2::Sha256::new()),remaining:3*1024*1024};
    serde_json::to_writer(&mut writer,value).map_err(|error|error.to_string())?;
    Ok(format!("{:x}",writer.inner.0.finalize()))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn read(db: &Connection, operation: Uuid) -> Result<Option<Obligation>, String> {
    let row = db.query_row(
        "SELECT request_digest,commit_digest,phase,CASE WHEN length(CAST(scope AS BLOB))<=1048576 THEN scope END,scope IS NOT NULL,committed_sequence,registry,CASE WHEN length(CAST(repository AS BLOB))<=16384 THEN repository END,project_ordinal FROM project_operation_obligations WHERE operation=?1",
        [operation.to_string()],
        |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,bool>(4)?,row.get::<_,Option<u64>>(5)?,row.get::<_,String>(6)?,row.get::<_,Option<String>>(7)?,row.get::<_,u32>(8)?)),
    ).optional().map_err(|error| error.to_string())?;
    row.map(|(request_digest,commit_digest,phase,scope,has_scope,committed_sequence,registry,repository,ordinal)| {
        if !valid_digest(&request_digest) || !valid_digest(&commit_digest) || has_scope != scope.is_some() {
            return Err("Unreadable project operation custody metadata; its scope remains unavailable".into());
        }
        let project=ProjectInstance{registry:registry.parse::<Uuid>().map_err(|error|error.to_string())?,repository:PathBuf::from(repository.ok_or("Unreadable project operation registry scope")?),ordinal};
        project.validate()?;
        let phase=Phase::parse(&phase)?;
        let scope=scope.map(|scope|serde_json::from_str::<SavedScope>(&scope).map_err(|error|error.to_string())).transpose()?;
        if let Some(scope)=&scope {
            scope.validate()?;
            if scope.project!=project {return Err("The original project-operation scope conflicts with its durable identity".into());}
        }
        if matches!(phase,Phase::Resolved|Phase::Aborted) != scope.is_none()
            || matches!(phase,Phase::ModelCommitted|Phase::Resolved) != committed_sequence.is_some() {
            return Err("Inconsistent project operation custody phase".into());
        }
        Ok(Obligation{operation,project,request_digest,commit_digest,phase,scope,committed_sequence})
    }).transpose()
}

/// Read the original bounded journal through the caller's actual transaction.
/// Recovery admission composes here, without opening a second connection or
/// reconstructing scope/roots from a new request. This grants no write/effect.
pub(crate) fn load_in(tx:&rusqlite::Transaction<'_>,operation:Uuid)->Result<Option<Obligation>,String> {
    if operation.is_nil() {return Err("Project operation identity is required".into());}
    read(tx,operation)
}
fn pending(db:&Connection)->Result<Vec<Obligation>,String> {
    let ids=db.prepare("SELECT operation FROM project_operation_obligations WHERE phase NOT IN ('resolved','aborted') ORDER BY rowid LIMIT 65").map_err(|error|error.to_string())?
        .query_map([],|row|row.get::<_,String>(0)).map_err(|error|error.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(|error|error.to_string())?;
    if ids.len()>MAX_PENDING {return Err("Project operation custody exceeds its pending limit; ordinary affected mutations remain unavailable".into());}
    ids.into_iter().map(|id|read(db,id.parse::<Uuid>().map_err(|error|error.to_string())?)?.ok_or("A pending operation disappeared".into())).collect()
}
pub(crate) fn pending_in(tx:&rusqlite::Transaction<'_>)->Result<Vec<Obligation>,String> {pending(tx)}

impl Registry {
    /// Pending/status diagnostics share one read snapshot and never migrate or
    /// acquire execution authority. Scope bodies are bounded private metadata.
    fn operation_snapshot<T>(&self,expected:Uuid,read:impl FnOnce(&rusqlite::Transaction<'_>)->Result<T,String>)->Result<T,String> {
        if expected.is_nil() {return Err("The original registry identity is required".into());}
        let tx=self.db.unchecked_transaction().map_err(|error|error.to_string())?;
        schema::check_readonly(&tx)?;
        let current:String=tx.query_row("SELECT identity FROM metadata WHERE key=0",[],|row|row.get(0)).map_err(|error|error.to_string())?;
        if current!=expected.to_string() {return Err("The original operation belongs to another workspace registry".into());}
        let result=read(&tx)?;tx.commit().map_err(|error|error.to_string())?;Ok(result)
    }
    pub(crate) fn project_operations_for_registry(&self,expected:Uuid)->Result<Vec<Obligation>,String> {
        self.project_operations_for_registry_with_retained(expected,&[])
    }
    /// Original daemon operation IDs only, never a client-supplied status scope.
    /// Include positive receipts whose exact heavy custody is still retained.
    pub(crate) fn project_operations_for_registry_with_retained(&self,expected:Uuid,retained:&[Uuid])->Result<Vec<Obligation>,String> {
        if retained.len()>crate::project_recovery::PROJECT_OPERATION_LIVE_LIMIT {return Err("Retained operation status capture exceeds its bound".into());}
        self.operation_snapshot(expected,|tx| {
            let mut rows=pending_in(tx)?;
            for operation in retained {
                if rows.iter().any(|row|row.operation==*operation) {continue;}
                let row=load_in(tx,*operation)?.ok_or("A retained original operation lost its journal receipt")?;
                rows.push(row);
                if rows.len()>crate::project_recovery::PROJECT_OPERATION_STATUS_LIMIT {return Err("Combined pending and retained operation status exceeds its bound; custody was preserved".into());}
            }
            if rows.iter().any(|row|row.project.registry!=expected) {return Err("The retained operation journal contains a different registry's scope".into());}
            Ok(rows)
        })
    }
    pub(crate) fn project_operation_for_registry(&self,expected:Uuid,operation:Uuid)->Result<Option<Obligation>,String> {
        self.operation_snapshot(expected,|tx| {
            let row=load_in(tx,operation)?;
            if row.as_ref().is_some_and(|row|row.project.registry!=expected) {return Err("The original operation receipt belongs to another registry".into());}
            Ok(row)
        })
    }
    pub(crate) fn project_operation(&self, operation: Uuid) -> Result<Option<Obligation>, String> {
        if operation.is_nil() {return Err("Project operation identity is required".into());}
        read(&self.db,operation)
    }
    /// Must complete BEFORE the first repair/close. Never evict unfinished rows
    /// or settled operation identities to make room for a new operation.
    pub(crate) fn begin_project_operation(&mut self, operation: Uuid, request_digest: &str,
        commit_digest: &str, scope: &SavedScope) -> Result<Obligation,String> {
        scope.validate()?;
        if operation.is_nil() || !valid_digest(request_digest) || !valid_digest(commit_digest) {
            return Err("Project operation identity or digest is invalid".into());
        }
        let scope_json=bounded_scope(scope)?;
        let tx=self.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|error|error.to_string())?;
        // Actual cumulative19 guard (Automation c413), inside this writer TX.
        schema::require_current_write(&tx)?;
        if let Some(existing)=read(&tx,operation)? {
            if existing.request_digest!=request_digest || existing.commit_digest!=commit_digest
                || existing.scope.as_ref().is_some_and(|original|original!=scope) {
                return Err("This project operation identity belongs to another original request or retained scope".into());
            }
            return Ok(existing);
        }
        project_instances::require(&tx,&scope.project)?;
        let (total,pending):(u64,u64)=tx.query_row("SELECT count(*),coalesce(sum(phase NOT IN ('resolved','aborted')),0) FROM project_operation_obligations",[],|row|Ok((row.get(0)?,row.get(1)?))).map_err(|error|error.to_string())?;
        if total>=MAX_OPERATIONS as u64 || pending>=MAX_PENDING as u64 {
            return Err("Project operation custody is at capacity. Existing operations were preserved; resolve them before starting another.".into());
        }
        tx.execute("INSERT INTO project_operation_obligations(operation,registry,repository,project_ordinal,request_digest,commit_digest,phase,scope) VALUES(?1,?2,?3,?4,?5,?6,'admitted',?7)",
            params![operation.to_string(),scope.project.registry.to_string(),repository_key(&scope.project.repository)?,scope.project.ordinal,request_digest,commit_digest,scope_json]).map_err(|error|error.to_string())?;
        tx.commit().map_err(|error|error.to_string())?;
        read(&self.db,operation)?.ok_or("The admitted operation was not retained".into())
    }
    pub(crate) fn advance_project_operation(&mut self, operation: Uuid, request_digest:&str,
        expected:Phase, next:Phase)->Result<(),String> {
        let allowed=matches!((expected,next),
            (Phase::Admitted,Phase::Repairing|Phase::Closing|Phase::PendingCommit|Phase::Aborted)
            | (Phase::Repairing|Phase::Closing,Phase::PendingCommit)
            | (Phase::ModelCommitted,Phase::Resolved));
        if !allowed {return Err("Invalid project operation phase transition".into());}
        let tx=self.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|error|error.to_string())?;
        schema::require_current_write(&tx)?;
        let changed=tx.execute("UPDATE project_operation_obligations SET phase=?4,scope=CASE WHEN ?4 IN ('resolved','aborted') THEN NULL ELSE scope END WHERE operation=?1 AND request_digest=?2 AND phase=?3",
            params![operation.to_string(),request_digest,expected.text(),next.text()]).map_err(|error|error.to_string())?;
        if changed!=1 {return Err("The original project operation custody phase changed".into());}
        tx.commit().map_err(|error|error.to_string())
    }
    /// Startup uses these as negative scopes only. No factory or path-based
    /// recreation of the original in-memory prepared phase is permitted.
    pub(crate) fn pending_project_operations(&self)->Result<Vec<Obligation>,String> {
        pending(&self.db)
    }
}

/// Called inside the SAME transaction as the exact guarded model edit. It is
/// not a second check-then-write boundary and cannot accept a different edit.
pub(super) fn model_committed(tx:&rusqlite::Transaction<'_>,operation:Uuid,
    request_digest:&str,edit:&ModelEdit)->Result<(),String> {
    schema::require_current_write(tx)?;
    let row=read(tx,operation)?.ok_or("The original project operation obligation is missing")?;
    if row.phase!=Phase::PendingCommit || row.request_digest!=request_digest || row.commit_digest!=digest(edit)? {
        return Err("The guarded model edit does not match its prepared project operation".into());
    }
    let sequence:u64=tx.query_row("SELECT sequence FROM metadata WHERE key=0",[],|row|row.get(0)).map_err(|error|error.to_string())?;
    let changed=tx.execute("UPDATE project_operation_obligations SET phase='model_committed',committed_sequence=?3 WHERE operation=?1 AND request_digest=?2 AND phase='pending_commit'",
        params![operation.to_string(),request_digest,sequence]).map_err(|error|error.to_string())?;
    if changed!=1 {return Err("The project operation lost its atomic model-commit custody".into());}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup()->(Registry,tempfile::TempDir,SavedScope) {
        // Actual cumulative future controller is mandatory; this fixture never
        // creates a test-only copy of its table or advances a private version.
        let dir=tempfile::tempdir().unwrap();
        let mut registry=Registry::at(&dir.path().join("registry.sqlite3")).unwrap();
        registry.apply(&ModelEdit::OpenProject{repository:"/project".into(),checkout:None,primary_checkout:None,name:"project".into(),kind:ProjectKind::Folder}).unwrap();
        let scope=SavedScope{project:registry.project_instance(Path::new("/project")).unwrap(),relocation:None,
            roots:vec!["/project".into()],protected:vec!["/unrelated".into()],channels:vec![],panes:vec![],sessions:vec![]};
        (registry,dir,scope)
    }
    #[test]
    fn unfinished_operation_survives_restart_without_becoming_execution_authority() {
        let (mut registry,dir,scope)=setup();
        let operation=Uuid::new_v4();let request=digest(&scope.project).unwrap();
        let edit=ModelEdit::CloseProjectInstance{project:scope.project.clone()};
        registry.begin_project_operation(operation,&request,&digest(&edit).unwrap(),&scope).unwrap();
        registry.advance_project_operation(operation,&request,Phase::Admitted,Phase::Closing).unwrap();
        drop(registry);
        let registry=Registry::at(&dir.path().join("registry.sqlite3")).unwrap();
        let pending=registry.pending_project_operations().unwrap();
        assert_eq!(pending.len(),1);assert_eq!(pending[0].operation,operation);
        assert_eq!(pending[0].phase,Phase::Closing);
        assert_eq!(pending[0].scope.as_ref().unwrap().project,scope.project);
        assert!(registry.model().unwrap().project(Path::new("/project")).is_some());
        // This row carries only negative metadata, not handles/FDs. The live
        // consumer must refuse execution/replay when original custody is absent.
    }
    #[test]
    fn operation_status_reads_original_identity_including_settled_receipts_and_refuses_wrong_registry() {
        let (mut registry,_,scope)=setup();let operation=Uuid::new_v4();
        let request=digest(&scope.project).unwrap();let edit=ModelEdit::CloseProjectInstance{project:scope.project.clone()};
        registry.begin_project_operation(operation,&request,&digest(&edit).unwrap(),&scope).unwrap();
        assert_eq!(registry.project_operations_for_registry(scope.project.registry).unwrap()[0].project,scope.project);
        assert!(registry.project_operations_for_registry(Uuid::nil()).is_err());
        assert!(registry.project_operations_for_registry(Uuid::new_v4()).is_err());
        assert!(registry.project_operation_for_registry(scope.project.registry,Uuid::nil()).is_err());
        assert!(registry.project_operation_for_registry(scope.project.registry,Uuid::new_v4()).unwrap().is_none());
        registry.advance_project_operation(operation,&request,Phase::Admitted,Phase::PendingCommit).unwrap();
        let tx=registry.db.transaction().unwrap();model_committed(&tx,operation,&request,&edit).unwrap();tx.commit().unwrap();
        registry.advance_project_operation(operation,&request,Phase::ModelCommitted,Phase::Resolved).unwrap();
        assert!(registry.project_operations_for_registry(scope.project.registry).unwrap().is_empty());
        let retained=registry.project_operations_for_registry_with_retained(scope.project.registry,&[operation,operation]).unwrap();
        assert_eq!(retained.len(),1);assert_eq!(retained[0].phase,Phase::Resolved);assert!(retained[0].scope.is_none());
        assert!(registry.project_operations_for_registry_with_retained(scope.project.registry,&[Uuid::nil()]).is_err());
        assert!(registry.project_operations_for_registry_with_retained(Uuid::new_v4(),&[operation]).is_err());
        let settled=registry.project_operation_for_registry(scope.project.registry,operation).unwrap().unwrap();
        assert_eq!(settled.project,scope.project);assert_eq!(settled.phase,Phase::Resolved);assert!(settled.scope.is_none());
        assert!(registry.project_operation_for_registry(Uuid::new_v4(),operation).is_err());
        // Even a syntactically valid row cannot substitute a different scope
        // identity into the bounded read snapshot.
        registry.db.execute("UPDATE project_operation_obligations SET registry=?2 WHERE operation=?1",params![operation.to_string(),Uuid::new_v4().to_string()]).unwrap();
        assert!(registry.project_operation_for_registry(scope.project.registry,operation).is_err());
    }
    #[test]
    fn full_pending_budget_cannot_hide_a_retained_positive_settlement_receipt() {
        let (mut registry,_,scope)=setup();let original=Uuid::new_v4();
        let request=digest(&scope.project).unwrap();let edit=ModelEdit::CloseProjectInstance{project:scope.project.clone()};
        let commit=digest(&edit).unwrap();
        registry.begin_project_operation(original,&request,&commit,&scope).unwrap();
        for _ in 1..MAX_PENDING {registry.begin_project_operation(Uuid::new_v4(),&request,&commit,&scope).unwrap();}
        registry.advance_project_operation(original,&request,Phase::Admitted,Phase::PendingCommit).unwrap();
        let tx=registry.db.transaction().unwrap();model_committed(&tx,original,&request,&edit).unwrap();tx.commit().unwrap();
        registry.advance_project_operation(original,&request,Phase::ModelCommitted,Phase::Resolved).unwrap();
        let refill=Uuid::new_v4();registry.begin_project_operation(refill,&request,&commit,&scope).unwrap();
        let rows=registry.project_operations_for_registry_with_retained(scope.project.registry,&[original]).unwrap();
        assert_eq!(rows.len(),MAX_PENDING+1);
        assert_eq!(rows.iter().filter(|row|row.phase==Phase::Resolved).count(),1);
        assert!(rows.iter().any(|row|row.operation==refill&&row.phase==Phase::Admitted));
        assert!(rows.iter().any(|row|row.operation==original&&row.scope.is_none()));
        assert_eq!(registry.pending_project_operations().unwrap().len(),MAX_PENDING);
        assert!(registry.begin_project_operation(Uuid::new_v4(),&request,&commit,&scope).is_err());
    }

    #[test]
    fn original_operation_cannot_be_rekeyed_or_advanced_by_a_different_request() {
        let (mut registry,_,scope)=setup();let operation=Uuid::new_v4();
        let request=digest(&scope.project).unwrap();let commit=digest(&ModelEdit::CloseProjectInstance{project:scope.project.clone()}).unwrap();
        registry.begin_project_operation(operation,&request,&commit,&scope).unwrap();
        let other=digest(&"other").unwrap();
        assert!(registry.begin_project_operation(operation,&other,&commit,&scope).is_err());
        let mut substituted=scope.clone();substituted.roots.push("/different".into());
        assert!(registry.begin_project_operation(operation,&request,&commit,&substituted).is_err(),"a replay cannot substitute fresh negative scope after matching only its digests");
        assert!(registry.advance_project_operation(operation,&other,Phase::Admitted,Phase::Closing).is_err());
        assert!(registry.advance_project_operation(operation,&request,Phase::Admitted,Phase::Resolved).is_err());
        assert_eq!(registry.project_operation(operation).unwrap().unwrap().phase,Phase::Admitted);
    }
    #[test]
    fn model_receipt_and_custody_phase_share_the_actual_sql_transaction() {
        let (mut registry,_,scope)=setup();let operation=Uuid::new_v4();
        let request=digest(&scope.project).unwrap();let edit=ModelEdit::CloseProjectInstance{project:scope.project.clone()};
        registry.begin_project_operation(operation,&request,&digest(&edit).unwrap(),&scope).unwrap();
        registry.advance_project_operation(operation,&request,Phase::Admitted,Phase::PendingCommit).unwrap();
        let tx=registry.db.transaction().unwrap();
        tx.execute("DELETE FROM projects WHERE repository='/project'",[]).unwrap();
        model_committed(&tx,operation,&request,&edit).unwrap();
        assert_eq!(load_in(&tx,operation).unwrap().unwrap().phase,Phase::ModelCommitted);
        assert_eq!(pending_in(&tx).unwrap()[0].phase,Phase::ModelCommitted);
        assert!(load_in(&tx,Uuid::nil()).is_err());
        tx.rollback().unwrap();
        assert!(registry.model().unwrap().project(Path::new("/project")).is_some());
        assert_eq!(registry.project_operation(operation).unwrap().unwrap().phase,Phase::PendingCommit);
        let tx=registry.db.transaction().unwrap();
        tx.execute("DELETE FROM projects WHERE repository='/project'",[]).unwrap();
        model_committed(&tx,operation,&request,&edit).unwrap();tx.commit().unwrap();
        registry.advance_project_operation(operation,&request,Phase::ModelCommitted,Phase::Resolved).unwrap();
        let settled=registry.project_operation(operation).unwrap().unwrap();
        assert_eq!(settled.phase,Phase::Resolved);assert!(settled.scope.is_none());
        assert!(settled.committed_sequence.is_some());assert!(registry.pending_project_operations().unwrap().is_empty());
        let replay=registry.begin_project_operation(operation,&request,&digest(&edit).unwrap(),&scope).unwrap();
        assert_eq!(replay.phase,Phase::Resolved,"settled replay must never recreate a lease or repeat effects");
    }
    #[test]
    fn retained_registry_rechecks_schema_inside_every_custody_write_transaction() {
        for future in [false,true] {
            let (mut registry,dir,scope)=setup();
            let operation=Uuid::new_v4();let request=digest(&scope.project).unwrap();
            let edit=ModelEdit::CloseProjectInstance{project:scope.project.clone()};
            registry.begin_project_operation(operation,&request,&digest(&edit).unwrap(),&scope).unwrap();
            registry.advance_project_operation(operation,&request,Phase::Admitted,Phase::PendingCommit).unwrap();
            let advancing=Uuid::new_v4();registry.begin_project_operation(advancing,&request,&digest(&edit).unwrap(),&scope).unwrap();
            let other=Connection::open(dir.path().join("registry.sqlite3")).unwrap();
            if future {other.pragma_update(None,"user_version",20).unwrap();}
            else {other.execute_batch("ALTER TABLE projects ADD COLUMN foreign_layout TEXT").unwrap();}
            let before:Vec<String>=registry.db.prepare("SELECT operation||':'||phase||':'||scope FROM project_operation_obligations ORDER BY operation").unwrap()
                .query_map([],|row|row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            assert!(registry.begin_project_operation(Uuid::new_v4(),&request,&digest(&edit).unwrap(),&scope).is_err());
            assert!(registry.advance_project_operation(advancing,&request,Phase::Admitted,Phase::Closing).is_err());
            // This separately exercises a valid transition, not only its phase validator.
            let tx=registry.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
            assert!(model_committed(&tx,operation,&request,&edit).is_err());
            tx.rollback().unwrap();
            let after:Vec<String>=registry.db.prepare("SELECT operation||':'||phase||':'||scope FROM project_operation_obligations ORDER BY operation").unwrap()
                .query_map([],|row|row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            assert_eq!(after,before,"a stale connection cannot publish a custody mutation");
        }
    }
    #[test]
    fn negative_scope_size_and_pending_capacity_refuse_without_evicting_originals() {
        let (mut registry,_,scope)=setup();let request=digest(&scope.project).unwrap();let commit=digest(&"commit").unwrap();
        let original=Uuid::new_v4();registry.begin_project_operation(original,&request,&commit,&scope).unwrap();
        for _ in 1..MAX_PENDING {registry.begin_project_operation(Uuid::new_v4(),&request,&commit,&scope).unwrap();}
        assert!(registry.begin_project_operation(Uuid::new_v4(),&request,&commit,&scope).is_err());
        assert_eq!(registry.pending_project_operations().unwrap().len(),MAX_PENDING);
        assert_eq!(registry.project_operation(original).unwrap().unwrap().phase,Phase::Admitted);
        let mut large=scope;large.roots=vec![PathBuf::from(format!("/{}","x".repeat(MAX_SCOPE_BYTES)))];
        assert!(registry.begin_project_operation(Uuid::new_v4(),&request,&commit,&large).is_err());
        assert!(registry.project_operation(original).unwrap().is_some());
        let mut aggregate=registry.project_operation(original).unwrap().unwrap().scope.unwrap();
        aggregate.roots=vec![PathBuf::from(format!("/{}","x".repeat(255)));8192];
        assert!(bounded_scope(&aggregate).is_err(),"combined serialization must stop at its bounded writer before allocating an oversized record");
        assert!(digest(&"x".repeat(3*1024*1024+1)).is_err());
    }
}
