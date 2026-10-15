//! Private semantic outcomes and stable input receipts. The live reducer and
//! transcript remain in the holder; this is never a stdout/reducer replay log.
use super::super::Update;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path};

const DEFAULT_BUDGET: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
const MAX_INPUTS: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OutboxEntry {
    pub sequence: u64,
    pub update: Update,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InputIdentity {
    pub sequence: u64,
    pub id: uuid::Uuid,
    pub hash: [u8; 32],
}

/// Accepted includes a queued or partially written provider command. It never
/// authorizes a retry. Acknowledged means the existing actor's reply, whose
/// precise semantics remain the provider adapter's contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum InputState {
    Accepted,
    Acknowledged(Result<(), String>),
    Unknown(String),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct InputReceipt {
    pub identity: InputIdentity,
    pub state: InputState,
}

pub(super) struct Journal {
    db: Connection,
    budget: usize,
    last_loss:Option<String>,
}
impl Journal {
    pub fn open(directory: &Path) -> Result<Self, String> {
        Self::with_budget(directory, DEFAULT_BUDGET)
    }
    fn with_budget(directory: &Path, budget: usize) -> Result<Self, String> {
        let path = directory.join("actor-journal.sqlite");
        OpenOptions::new().read(true).write(true).create_new(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path)
            .map_err(|error| format!("Cannot create private actor journal: {error}"))?;
        let db = Connection::open(path).map_err(|error| error.to_string())?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
            CREATE TABLE outcomes(sequence INTEGER PRIMARY KEY AUTOINCREMENT,payload BLOB NOT NULL);
            CREATE TABLE retained_runs(run TEXT PRIMARY KEY,finished INTEGER);
            CREATE TABLE inputs(sequence INTEGER PRIMARY KEY,id TEXT UNIQUE NOT NULL,hash BLOB NOT NULL,state BLOB NOT NULL);
            CREATE TABLE metadata(key TEXT PRIMARY KEY,value BLOB NOT NULL);
            INSERT INTO metadata VALUES('input_sequence','0'),('outcome_ack','0');")
            .map_err(|error| error.to_string())?;
        Ok(Self { db, budget,last_loss:None })
    }
    pub fn loss(&self) -> Result<Option<String>, String> {
        if let Some(loss)=&self.last_loss {return Ok(Some(loss.clone()));}
        self.db.query_row("SELECT value FROM metadata WHERE key='loss'",[],|row|row.get(0))
            .optional().map_err(|error|error.to_string())
    }
    pub fn record_loss(&mut self, reason: &str) -> Result<(), String> {
        if self.last_loss.is_none() {self.last_loss=Some(reason.into());}
        self.db.execute("INSERT OR IGNORE INTO metadata(key,value) VALUES('loss',?1)",[reason])
            .map_err(|error|error.to_string())?;
        Ok(())
    }
    pub fn append(&mut self, update: &Update) -> Result<(), String> {
        // These can change once per streaming frame. Their authoritative
        // bounded state is queried, so only durable outcomes enter the outbox.
        if !matches!(update,Update::Execution {..}|Update::Attention(_)) { return Ok(()); }
        if self.loss()?.is_some() { return Err("Actor outcome journal already has a recorded loss".into()); }
        let bytes = serde_json::to_vec(update).map_err(|error|error.to_string())?;
        let (count, retained):(usize,usize)=self.db.query_row(
            "SELECT COUNT(*),COALESCE(SUM(LENGTH(payload)),0) FROM outcomes",[],|row|Ok((row.get(0)?,row.get(1)?)))
            .map_err(|error|error.to_string())?;
        if count>=MAX_ENTRIES || bytes.len()>1024*1024 || retained.saturating_add(bytes.len())>self.budget {
            let reason="Claude holder outcome journal overflowed before the daemon committed its updates; the in-flight run outcome is unconfirmed";
            self.record_loss(reason)?;
            return Err(reason.into());
        }
        if let Update::Execution {run,..}=update {self.check_run_capacity(*run)?;}
        let tx=self.db.transaction().map_err(|error|error.to_string())?;
        tx.execute("INSERT INTO outcomes(payload) VALUES(?1)",[bytes]).map_err(|error|error.to_string())?;
        if let Update::Execution {run,update}=update {
            let finished=matches!(update,crate::agent_execution::Update::Finished{..}).then(||tx.last_insert_rowid());
            tx.execute("INSERT INTO retained_runs(run,finished) VALUES(?1,?2) ON CONFLICT(run) DO UPDATE SET finished=COALESCE(excluded.finished,retained_runs.finished)",params![run.to_string(),finished]).map_err(|error|error.to_string())?;
        }
        tx.commit().map_err(|error|error.to_string())
    }
    pub fn page(&self, after:u64) -> Result<Vec<OutboxEntry>,String> {
        let ack=self.number("outcome_ack")?;
        if after<ack { return Err("Outcome cursor predates already committed updates".into()); }
        let after=i64::try_from(after).map_err(|_|"Outcome cursor is out of range")?;
        let mut query=self.db.prepare("SELECT sequence,payload FROM outcomes WHERE sequence>?1 ORDER BY sequence LIMIT 64")
            .map_err(|error|error.to_string())?;
        let rows=query.query_map([after],|row|Ok((row.get::<_,u64>(0)?,row.get::<_,Vec<u8>>(1)?)))
            .map_err(|error|error.to_string())?;
        let mut result=Vec::new();
        let mut bytes=0usize;
        for row in rows {
            let (sequence,payload)=row.map_err(|error|error.to_string())?;
            if !result.is_empty() && bytes.saturating_add(payload.len())>1024*1024 { break; }
            bytes+=payload.len();
            result.push(OutboxEntry {sequence,update:serde_json::from_slice(&payload).map_err(|error|error.to_string())?});
        }
        Ok(result)
    }
    /// The authenticated daemon calls this only after its idempotent durable
    /// registry/notification transaction commits. Socket delivery is not an ACK.
    pub fn ack(&mut self, through:u64) -> Result<(),String> {
        let previous=self.number("outcome_ack")?;
        if through<previous { return Err("Outcome acknowledgement moved backwards".into()); }
        let latest:u64=self.db.query_row("SELECT COALESCE(MAX(sequence),?1) FROM outcomes",[previous],|row|row.get(0))
            .map_err(|error|error.to_string())?;
        if through>latest { return Err("Outcome acknowledgement exceeds the published sequence".into()); }
        let tx=self.db.transaction().map_err(|error|error.to_string())?;
        tx.execute("DELETE FROM retained_runs WHERE finished IS NOT NULL AND finished<=?1",[through]).map_err(|error|error.to_string())?;
        tx.execute("DELETE FROM outcomes WHERE sequence<=?1",[through]).map_err(|error|error.to_string())?;
        tx.execute("UPDATE metadata SET value=?1 WHERE key='outcome_ack'",[through.to_string()]).map_err(|error|error.to_string())?;
        tx.commit().map_err(|error|error.to_string())
    }
    pub fn input(&self, identity:&InputIdentity) -> Result<Option<InputReceipt>,String> {
        let stored:Option<(String,Vec<u8>,Vec<u8>)>=self.db.query_row("SELECT id,hash,state FROM inputs WHERE sequence=?1",[identity.sequence],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))
            .optional().map_err(|error|error.to_string())?;
        if let Some((id,hash,state))=stored {
            if id!=identity.id.to_string() || hash!=identity.hash { return Err("Input sequence was already used for a different command".into()); }
            return Ok(Some(InputReceipt {identity:identity.clone(),state:serde_json::from_slice(&state).map_err(|error|error.to_string())?}));
        }
        if identity.sequence<=self.number("input_sequence")? { return Err("Input receipt is retired or unavailable; never replay this command".into()); }
        Ok(None)
    }
    fn check_run_capacity(&self,run:crate::model::RunId)->Result<(),String> {
        let (count,exists):(usize,bool)=self.db.query_row("SELECT COUNT(*),EXISTS(SELECT 1 FROM retained_runs WHERE run=?1) FROM retained_runs",[run.to_string()],|row|Ok((row.get(0)?,row.get(1)?))).map_err(|error|error.to_string())?;
        if count>=MAX_ENTRIES && !exists {return Err("Retained actor run identity capacity reached; outcome continuity is unconfirmed".into());}Ok(())
    }
    pub fn retained_runs(&self)->Result<Vec<crate::model::RunId>,String> {
        let mut query=self.db.prepare("SELECT run FROM retained_runs ORDER BY run").map_err(|error|error.to_string())?;
        let rows=query.query_map([],|row|row.get::<_,String>(0)).map_err(|error|error.to_string())?;
        rows.map(|row|row.map_err(|error|error.to_string())?.parse().map_err(|_|"Invalid retained run identity".to_owned())).collect()
    }
    pub fn refuse_unexecuted_run(&mut self,run:crate::model::RunId)->Result<(),String> {
        self.db.execute("DELETE FROM retained_runs WHERE run=?1",[run.to_string()]).map_err(|error|error.to_string())?;Ok(())
    }
    pub fn admit(&mut self,identity:&InputIdentity) -> Result<InputReceipt,String> {self.admit_with_run(identity,None)}
    pub fn admit_with_run(&mut self,identity:&InputIdentity,run:Option<crate::model::RunId>) -> Result<InputReceipt,String> {
        if let Some(receipt)=self.input(identity)? { return Ok(receipt); }
        if self.loss()?.is_some() { return Err("Actor outcome continuity is unconfirmed; new input is refused".into()); }
        if identity.sequence!=self.number("input_sequence")?.checked_add(1).ok_or("Input sequence exhausted")? {
            return Err("Input sequence must be the next contiguous sequence".into());
        }
        let count:usize=self.db.query_row("SELECT COUNT(*) FROM inputs",[],|row|row.get(0)).map_err(|error|error.to_string())?;
        if count>=MAX_INPUTS { return Err("Actor input receipt capacity reached; commit completed receipts before sending more".into()); }
        if let Some(run)=run {self.check_run_capacity(run)?;}
        let state=InputState::Accepted;
        let tx=self.db.transaction().map_err(|error|error.to_string())?;
        tx.execute("INSERT INTO inputs(sequence,id,hash,state) VALUES(?1,?2,?3,?4)",params![identity.sequence,identity.id.to_string(),identity.hash.as_slice(),serde_json::to_vec(&state).map_err(|error|error.to_string())?]).map_err(|error|error.to_string())?;
        if let Some(run)=run {tx.execute("INSERT OR IGNORE INTO retained_runs(run,finished) VALUES(?1,NULL)",[run.to_string()]).map_err(|error|error.to_string())?;}
        tx.execute("UPDATE metadata SET value=?1 WHERE key='input_sequence'",[identity.sequence.to_string()]).map_err(|error|error.to_string())?;
        tx.commit().map_err(|error|error.to_string())?;
        Ok(InputReceipt {identity:identity.clone(),state})
    }
    pub fn finish(&mut self,identity:&InputIdentity,mut state:InputState)->Result<(),String> {
        let error=match &mut state { InputState::Acknowledged(Err(error))|InputState::Unknown(error)=>Some(error),_=>None };
        if let Some(error)=error && error.len()>4096 { let mut end=4096;while !error.is_char_boundary(end) {end-=1;}error.truncate(end); }
        let receipt=self.input(identity)?.ok_or("Input receipt does not exist")?;
        if receipt.state!=InputState::Accepted { return Err("Input receipt already has an outcome".into()); }
        self.db.execute("UPDATE inputs SET state=?1 WHERE sequence=?2",params![serde_json::to_vec(&state).map_err(|error|error.to_string())?,identity.sequence]).map_err(|error|error.to_string())?;
        Ok(())
    }
    pub fn retire_inputs(&mut self,through:u64)->Result<(),String> {
        let mut query=self.db.prepare("SELECT state FROM inputs WHERE sequence<=?1").map_err(|error|error.to_string())?;
        let rows=query.query_map([through],|row|row.get::<_,Vec<u8>>(0)).map_err(|error|error.to_string())?;
        for row in rows {
            let state:InputState=serde_json::from_slice(&row.map_err(|error|error.to_string())?).map_err(|error|error.to_string())?;
            if !matches!(state,InputState::Acknowledged(_)) { return Err("Cannot retire an input without its acknowledged outcome".into()); }
        }
        drop(query);
        self.db.execute("DELETE FROM inputs WHERE sequence<=?1",[through]).map_err(|error|error.to_string())?;
        Ok(())
    }
    pub fn next_input(&self)->Result<u64,String> {self.number("input_sequence")?.checked_add(1).ok_or("Input sequence exhausted".into())}
    pub fn acknowledged(&self)->Result<u64,String> {self.number("outcome_ack")}
    pub fn latest(&self)->Result<u64,String> {let ack=self.acknowledged()?;self.db.query_row("SELECT COALESCE(MAX(sequence),?1) FROM outcomes",[ack],|row|row.get(0)).map_err(|error|error.to_string())}
    fn number(&self,key:&str)->Result<u64,String> {
        let value:String=self.db.query_row("SELECT value FROM metadata WHERE key=?1",[key],|row|row.get(0)).map_err(|error|error.to_string())?;
        value.parse().map_err(|_|"Invalid actor journal sequence".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepted_run_survives_input_ack_and_finished_delivery_until_durable_outcome_ack() {
        let dir=tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal=Journal::with_budget(dir.path(),4096).unwrap();
        let run=crate::model::RunId::new();
        let identity=InputIdentity {sequence:1,id:uuid::Uuid::new_v4(),hash:[7;32]};
        journal.admit_with_run(&identity,Some(run)).unwrap();
        assert_eq!(journal.retained_runs().unwrap(),vec![run]);
        journal.finish(&identity,InputState::Acknowledged(Ok(()))).unwrap();journal.retire_inputs(1).unwrap();
        journal.append(&Update::Execution {run,update:crate::agent_execution::Update::Started {provider_key:"turn".into()}}).unwrap();
        journal.ack(1).unwrap();assert_eq!(journal.retained_runs().unwrap(),vec![run]);
        journal.append(&Update::Execution {run,update:crate::agent_execution::Update::Finished {state:crate::agent_execution::State::Succeeded,result:Default::default()}}).unwrap();
        assert_eq!(journal.retained_runs().unwrap(),vec![run],"delivery to a proxy is not a durable registry ACK");
        journal.ack(2).unwrap();assert!(journal.retained_runs().unwrap().is_empty());
    }
    #[test]
    fn receipts_never_readmit_duplicate_or_retired_inputs() {
        let dir=tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal=Journal::with_budget(dir.path(),1024).unwrap();
        let identity=InputIdentity {sequence:1,id:uuid::Uuid::new_v4(),hash:[7;32]};
        assert_eq!(journal.admit(&identity).unwrap().state,InputState::Accepted);
        assert_eq!(journal.admit(&identity).unwrap().state,InputState::Accepted);
        let mut changed=identity.clone();changed.hash=[8;32];assert!(journal.admit(&changed).is_err());
        journal.finish(&identity,InputState::Acknowledged(Ok(()))).unwrap();
        journal.retire_inputs(1).unwrap();
        assert!(journal.admit(&identity).is_err());
        let mut next=identity.clone();next.sequence=3;next.id=uuid::Uuid::new_v4();assert!(journal.admit(&next).is_err());
        next.sequence=2;assert!(journal.admit(&next).is_ok());
    }
    #[test]
    fn overflow_is_durable_and_ack_does_not_hide_the_loss() {
        let dir=tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal=Journal::with_budget(dir.path(),1).unwrap();
        let update=Update::Attention(crate::notifications::Evidence {key:"turn".into(),kind:crate::notifications::Kind::Succeeded});
        assert!(journal.append(&update).is_err());
        assert!(journal.loss().unwrap().unwrap().contains("overflowed"));
        journal.ack(0).unwrap();
        assert!(journal.loss().unwrap().is_some());
    }
    #[test]
    fn outbox_repeats_until_commit_ack_and_rejects_future_ack() {
        let dir=tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal=Journal::with_budget(dir.path(),4096).unwrap();
        journal.append(&Update::Attention(crate::notifications::Evidence {key:"turn".into(),kind:crate::notifications::Kind::Waiting})).unwrap();
        assert_eq!(journal.page(0).unwrap().len(),1);
        assert_eq!(journal.page(0).unwrap().len(),1);
        assert!(journal.ack(2).is_err());
        journal.ack(1).unwrap();
        assert!(journal.page(0).is_err());
        assert!(journal.page(1).unwrap().is_empty());
    }
}
