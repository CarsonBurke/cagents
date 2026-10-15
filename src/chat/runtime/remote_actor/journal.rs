//! Private semantic outcomes and stable input receipts. The live reducer and
//! transcript remain in the holder; this is never a stdout/reducer replay log.
use super::super::Update;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path};

const DEFAULT_BUDGET: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
pub(super) const MAX_INPUTS: usize = 1024;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContinuityLoss {
    pub id: uuid::Uuid,
    pub affected_runs: Vec<crate::model::RunId>,
    pub reason: String,
    pub acknowledged: bool,
}
pub(super) struct Journal {
    db: Connection,
    budget: usize,
    last_loss: Option<ContinuityLoss>,
    run_ids: std::collections::BTreeSet<crate::model::RunId>,
    finished_runs: std::collections::BTreeMap<crate::model::RunId, u64>,
    input_sequence: u64,
    acknowledged_inputs: std::collections::BTreeSet<u64>,
    outcome_ack: u64,
    outcome_latest: u64,
}
/// Context is optional enrichment. Invalid enrichment must not erase the
/// independently authoritative evidence or block later execution settlements.
fn valid_attention_context(
    evidence: &crate::notifications::Evidence,
    content: &crate::notifications::Content,
) -> bool {
    let bounded = |text: &str, limit| {
        text.chars().take(limit + 1).count() <= limit && !text.chars().any(char::is_control)
    };
    bounded(&content.conversation, 80)
        && bounded(&content.preview, 240)
        && (content.request.is_none() || evidence.kind == crate::notifications::Kind::Waiting)
}
impl Journal {
    pub fn open(directory: &Path) -> Result<Self, String> {
        Self::with_budget(directory, DEFAULT_BUDGET)
    }
    fn with_budget(directory: &Path, budget: usize) -> Result<Self, String> {
        let path = directory.join("actor-journal.sqlite");
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| format!("Cannot create private actor journal: {error}"))?;
        let db = Connection::open(path).map_err(|error| error.to_string())?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
            CREATE TABLE outcomes(sequence INTEGER PRIMARY KEY AUTOINCREMENT,payload BLOB NOT NULL);
            CREATE TABLE restarts(id TEXT PRIMARY KEY,hash BLOB NOT NULL,state BLOB NOT NULL);
            CREATE TABLE retained_runs(run TEXT PRIMARY KEY,finished INTEGER);
            CREATE TABLE inputs(sequence INTEGER PRIMARY KEY,id TEXT UNIQUE NOT NULL,hash BLOB NOT NULL,state BLOB NOT NULL);
            CREATE TABLE metadata(key TEXT PRIMARY KEY,value BLOB NOT NULL);
            INSERT INTO metadata VALUES('input_sequence','0'),('outcome_ack','0');")
            .map_err(|error| error.to_string())?;
        Ok(Self {
            db,
            budget,
            last_loss: None,
            run_ids: Default::default(),
            finished_runs: Default::default(),
            input_sequence: 0,
            acknowledged_inputs: Default::default(),
            outcome_ack: 0,
            outcome_latest: 0,
        })
    }
    pub fn continuity_loss(&self) -> Result<Option<ContinuityLoss>, String> {
        Ok(self.last_loss.clone())
    }
    pub fn loss(&self) -> Result<Option<String>, String> {
        Ok(self.continuity_loss()?.map(|loss| loss.reason))
    }
    pub fn record_loss(&mut self, reason: &str) -> Result<(), String> {
        if self.last_loss.is_none() {
            self.last_loss = Some(ContinuityLoss {
                id: uuid::Uuid::new_v4(),
                affected_runs: self.run_ids.iter().copied().collect(),
                reason: reason.chars().take(4096).collect(),
                acknowledged: false,
            });
        }
        let loss = self.last_loss.as_ref().unwrap();
        self.db
            .execute(
                "INSERT OR IGNORE INTO metadata(key,value) VALUES('continuity_loss',?1)",
                [serde_json::to_vec(loss).map_err(|error| error.to_string())?],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }
    pub fn ack_loss(&mut self, id: uuid::Uuid) -> Result<(), String> {
        let mut loss = self
            .continuity_loss()?
            .ok_or("No retained continuity loss exists")?;
        if loss.id != id {
            return Err("Continuity loss acknowledgement names another incident".into());
        }
        loss.acknowledged = true;
        self.db.execute("INSERT INTO metadata(key,value) VALUES('continuity_loss',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[serde_json::to_vec(&loss).map_err(|error|error.to_string())?]).map_err(|error|error.to_string())?;
        self.last_loss = Some(loss);
        Ok(())
    }
    pub fn clear_acknowledged_loss_for_restart(&mut self) -> Result<(), String> {
        let Some(loss) = self.continuity_loss()? else {
            return Ok(());
        };
        if !loss.acknowledged {
            return Err("The host has not durably acknowledged the uncertain run outcomes; explicit restart remains fenced".into());
        }
        if self.acknowledged()? != self.latest()? {
            return Err("Retained semantic outcomes must be durably drained before explicit restart clears this loss".into());
        }
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        for run in &loss.affected_runs {
            tx.execute("DELETE FROM retained_runs WHERE run=?1", [run.to_string()])
                .map_err(|error| error.to_string())?;
        }
        tx.execute("DELETE FROM metadata WHERE key='continuity_loss'", [])
            .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        for run in loss.affected_runs {
            self.run_ids.remove(&run);
            self.finished_runs.remove(&run);
        }
        self.last_loss = None;
        Ok(())
    }
    pub fn append(&mut self, update: &Update) -> Result<(), String> {
        // These can change once per streaming frame. Their authoritative
        // bounded state is queried, so only durable outcomes enter the outbox.
        if !matches!(
            update,
            Update::Execution { .. } | Update::Attention(_) | Update::AttentionContext { .. } | Update::TurnOutcome { .. }
        ) {
            return Ok(());
        }
        if let Update::TurnOutcome {update}=update {
            // Native turn/input identities retain their exact source proof.
            // They never settle or remove a managed current RunId by inference.
            update.validate()?;
        }
        // Keep bounded supplied context byte-for-byte. Never consult a later
        // snapshot or modify evidence to repair malformed optional display data.
        let metadata_only = match update {
            Update::AttentionContext {
                evidence,
                content: Some(content),
            } if !valid_attention_context(evidence, content) => Some(Update::AttentionContext {
                evidence: evidence.clone(),
                content: None,
            }),
            _ => None,
        };
        let update = metadata_only.as_ref().unwrap_or(update);
        if self.loss()?.is_some() {
            return Err("Actor outcome journal already has a recorded loss".into());
        }
        if let Update::Execution { run, .. } = update {
            if self.run_ids.len() >= MAX_ENTRIES && !self.run_ids.contains(run) {
                self.run_ids.insert(*run);
                self.record_loss("Retained actor run identity capacity reached; outcome continuity is unconfirmed")?;
                return Err("Retained actor run identity capacity reached".into());
            }
            self.run_ids.insert(*run);
        }
        let bytes = serde_json::to_vec(update).map_err(|error| error.to_string())?;
        let (count, retained): (usize, usize) = self
            .db
            .query_row(
                "SELECT COUNT(*),COALESCE(SUM(LENGTH(payload)),0) FROM outcomes",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| error.to_string())?;
        if count >= MAX_ENTRIES
            || bytes.len() > 1024 * 1024
            || retained.saturating_add(bytes.len()) > self.budget
        {
            let reason = "Claude holder outcome journal overflowed before the daemon committed its updates; the in-flight run outcome is unconfirmed";
            self.record_loss(reason)?;
            return Err(reason.into());
        }
        if let Update::Execution { run, .. } = update {
            self.check_run_capacity(*run)?;
        }
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        tx.execute("INSERT INTO outcomes(payload) VALUES(?1)", [bytes])
            .map_err(|error| error.to_string())?;
        let sequence = tx.last_insert_rowid() as u64;
        if let Update::Execution { run, update } = update {
            let finished = matches!(update, crate::agent_execution::Update::Finished { .. })
                .then(|| tx.last_insert_rowid());
            tx.execute("INSERT INTO retained_runs(run,finished) VALUES(?1,?2) ON CONFLICT(run) DO UPDATE SET finished=COALESCE(excluded.finished,retained_runs.finished)",params![run.to_string(),finished]).map_err(|error|error.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())?;
        self.outcome_latest = sequence;
        if let Update::Execution {
            run,
            update: crate::agent_execution::Update::Finished { .. },
        } = update
        {
            self.finished_runs.insert(*run, sequence);
        }
        Ok(())
    }
    pub fn page(&self, after: u64) -> Result<Vec<OutboxEntry>, String> {
        let ack = self.number("outcome_ack")?;
        if after < ack {
            return Err("Outcome cursor predates already committed updates".into());
        }
        let after = i64::try_from(after).map_err(|_| "Outcome cursor is out of range")?;
        let mut query=self.db.prepare("SELECT sequence,payload FROM outcomes WHERE sequence>?1 ORDER BY sequence LIMIT 64")
            .map_err(|error|error.to_string())?;
        let rows = query
            .query_map([after], |row| {
                Ok((row.get::<_, u64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|error| error.to_string())?;
        let mut result = Vec::new();
        let mut bytes = 0usize;
        for row in rows {
            let (sequence, payload) = row.map_err(|error| error.to_string())?;
            if !result.is_empty() && bytes.saturating_add(payload.len()) > 1024 * 1024 {
                break;
            }
            bytes += payload.len();
            result.push(OutboxEntry {
                sequence,
                update: serde_json::from_slice(&payload).map_err(|error| error.to_string())?,
            });
        }
        Ok(result)
    }
    /// The authenticated daemon calls this only after its idempotent durable
    /// registry/notification transaction commits. Socket delivery is not an ACK.
    pub fn ack(&mut self, through: u64) -> Result<(), String> {
        let previous = self.number("outcome_ack")?;
        if through < previous {
            return Err("Outcome acknowledgement moved backwards".into());
        }
        let latest: u64 = self
            .db
            .query_row(
                "SELECT COALESCE(MAX(sequence),?1) FROM outcomes",
                [previous],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if through > latest {
            return Err("Outcome acknowledgement exceeds the published sequence".into());
        }
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        tx.execute(
            "DELETE FROM retained_runs WHERE finished IS NOT NULL AND finished<=?1",
            [through],
        )
        .map_err(|error| error.to_string())?;
        tx.execute("DELETE FROM outcomes WHERE sequence<=?1", [through])
            .map_err(|error| error.to_string())?;
        tx.execute(
            "UPDATE metadata SET value=?1 WHERE key='outcome_ack'",
            [through.to_string()],
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        self.outcome_ack = through;
        self.finished_runs.retain(|run, sequence| {
            if *sequence <= through {
                self.run_ids.remove(run);
                false
            } else {
                true
            }
        });
        Ok(())
    }
    pub fn input(&self, identity: &InputIdentity) -> Result<Option<InputReceipt>, String> {
        let stored: Option<(String, Vec<u8>, Vec<u8>)> = self
            .db
            .query_row(
                "SELECT id,hash,state FROM inputs WHERE sequence=?1",
                [identity.sequence],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some((id, hash, state)) = stored {
            if id != identity.id.to_string() || hash != identity.hash {
                return Err("Input sequence was already used for a different command".into());
            }
            return Ok(Some(InputReceipt {
                identity: identity.clone(),
                state: serde_json::from_slice(&state).map_err(|error| error.to_string())?,
            }));
        }
        if identity.sequence <= self.number("input_sequence")? {
            return Err(
                "Input receipt is retired or unavailable; never replay this command".into(),
            );
        }
        Ok(None)
    }
    fn check_run_capacity(&self, run: crate::model::RunId) -> Result<(), String> {
        if self.run_ids.len() >= MAX_ENTRIES && !self.run_ids.contains(&run) {
            return Err(
                "Retained actor run identity capacity reached; outcome continuity is unconfirmed"
                    .into(),
            );
        }
        Ok(())
    }
    pub fn retained_runs(&self) -> Result<Vec<crate::model::RunId>, String> {
        Ok(self.run_ids.iter().copied().collect())
    }
    pub fn refuse_unexecuted_run(&mut self, run: crate::model::RunId) -> Result<(), String> {
        self.db
            .execute("DELETE FROM retained_runs WHERE run=?1", [run.to_string()])
            .map_err(|error| error.to_string())?;
        self.run_ids.remove(&run);
        self.finished_runs.remove(&run);
        Ok(())
    }
    pub fn admit(&mut self, identity: &InputIdentity) -> Result<InputReceipt, String> {
        self.admit_with_run(identity, None)
    }
    pub fn admit_with_run(
        &mut self,
        identity: &InputIdentity,
        run: Option<crate::model::RunId>,
    ) -> Result<InputReceipt, String> {
        if let Some(receipt) = self.input(identity)? {
            return Ok(receipt);
        }
        if self.loss()?.is_some() {
            return Err("Actor outcome continuity is unconfirmed; new input is refused".into());
        }
        if identity.sequence
            != self
                .number("input_sequence")?
                .checked_add(1)
                .ok_or("Input sequence exhausted")?
        {
            return Err("Input sequence must be the next contiguous sequence".into());
        }
        let count: usize = self
            .db
            .query_row("SELECT COUNT(*) FROM inputs", [], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if count >= MAX_INPUTS {
            return Err("Actor input receipt capacity reached; commit completed receipts before sending more".into());
        }
        if let Some(run) = run {
            self.check_run_capacity(run)?;
        }
        let state = InputState::Accepted;
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        tx.execute(
            "INSERT INTO inputs(sequence,id,hash,state) VALUES(?1,?2,?3,?4)",
            params![
                identity.sequence,
                identity.id.to_string(),
                identity.hash.as_slice(),
                serde_json::to_vec(&state).map_err(|error| error.to_string())?
            ],
        )
        .map_err(|error| error.to_string())?;
        if let Some(run) = run {
            tx.execute(
                "INSERT OR IGNORE INTO retained_runs(run,finished) VALUES(?1,NULL)",
                [run.to_string()],
            )
            .map_err(|error| error.to_string())?;
        }
        tx.execute(
            "UPDATE metadata SET value=?1 WHERE key='input_sequence'",
            [identity.sequence.to_string()],
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        self.input_sequence = identity.sequence;
        if let Some(run) = run {
            self.run_ids.insert(run);
        }
        Ok(InputReceipt {
            identity: identity.clone(),
            state,
        })
    }
    pub fn finish(
        &mut self,
        identity: &InputIdentity,
        mut state: InputState,
    ) -> Result<(), String> {
        let error = match &mut state {
            InputState::Acknowledged(Err(error)) | InputState::Unknown(error) => Some(error),
            _ => None,
        };
        if let Some(error) = error
            && error.len() > 4096
        {
            let mut end = 4096;
            while !error.is_char_boundary(end) {
                end -= 1;
            }
            error.truncate(end);
        }
        let receipt = self
            .input(identity)?
            .ok_or("Input receipt does not exist")?;
        if receipt.state != InputState::Accepted {
            return Err("Input receipt already has an outcome".into());
        }
        self.db
            .execute(
                "UPDATE inputs SET state=?1 WHERE sequence=?2",
                params![
                    serde_json::to_vec(&state).map_err(|error| error.to_string())?,
                    identity.sequence
                ],
            )
            .map_err(|error| error.to_string())?;
        if matches!(state, InputState::Acknowledged(_)) {
            self.acknowledged_inputs.insert(identity.sequence);
        }
        Ok(())
    }
    pub fn acknowledged_input_count(&self) -> usize {
        self.acknowledged_inputs.len()
    }
    pub fn acknowledged_input_page(&self) -> Result<Vec<InputReceipt>, String> {
        let mut page = Vec::new();
        for sequence in self.acknowledged_inputs.iter().take(64) {
            let (id, hash, state): (String, Vec<u8>, Vec<u8>) = self
                .db
                .query_row(
                    "SELECT id,hash,state FROM inputs WHERE sequence=?1",
                    [sequence],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(|error| error.to_string())?;
            let identity = InputIdentity {
                sequence: *sequence,
                id: uuid::Uuid::parse_str(&id).map_err(|error| error.to_string())?,
                hash: hash
                    .try_into()
                    .map_err(|_| "Invalid retained input digest")?,
            };
            let state = serde_json::from_slice(&state).map_err(|error| error.to_string())?;
            if !matches!(state, InputState::Acknowledged(_)) {
                return Err("Input acknowledgement cache lost its terminal-state proof".into());
            }
            page.push(InputReceipt { identity, state });
        }
        Ok(page)
    }
    /// Retire exact terminal receipts independently: an earlier unknown or
    /// partially written input must not block cleanup of unrelated outcomes.
    /// The durable high-water sequence continues refusing all retired replay.
    pub fn retire_acknowledged_inputs(
        &mut self,
        identities: &[InputIdentity],
    ) -> Result<(), String> {
        if identities.len() > 64 {
            return Err("Input retirement batch exceeds capacity".into());
        }
        for identity in identities {
            let row: Option<(String, Vec<u8>, Vec<u8>)> = self
                .db
                .query_row(
                    "SELECT id,hash,state FROM inputs WHERE sequence=?1",
                    [identity.sequence],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            match row {
                Some((id, hash, state)) => {
                    if id != identity.id.to_string() || hash.as_slice() != identity.hash.as_slice()
                    {
                        return Err(
                            "Input retirement identity differs from its retained command".into(),
                        );
                    }
                    if !matches!(
                        serde_json::from_slice::<InputState>(&state)
                            .map_err(|error| error.to_string())?,
                        InputState::Acknowledged(_)
                    ) {
                        return Err("Accepted or unknown inputs must retain their receipt".into());
                    }
                }
                None if identity.sequence > 0 && identity.sequence <= self.input_sequence => {}
                None => return Err("Input retirement exceeds the admitted sequence".into()),
            }
        }
        let transaction = self.db.transaction().map_err(|error| error.to_string())?;
        for identity in identities {
            transaction
                .execute("DELETE FROM inputs WHERE sequence=?1", [identity.sequence])
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())?;
        for identity in identities {
            self.acknowledged_inputs.remove(&identity.sequence);
        }
        Ok(())
    }

    pub fn retire_inputs(&mut self, through: u64) -> Result<(), String> {
        let mut query = self
            .db
            .prepare("SELECT state FROM inputs WHERE sequence<=?1")
            .map_err(|error| error.to_string())?;
        let rows = query
            .query_map([through], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|error| error.to_string())?;
        for row in rows {
            let state: InputState =
                serde_json::from_slice(&row.map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            if !matches!(state, InputState::Acknowledged(_)) {
                return Err("Cannot retire an input without its acknowledged outcome".into());
            }
        }
        drop(query);
        self.db
            .execute("DELETE FROM inputs WHERE sequence<=?1", [through])
            .map_err(|error| error.to_string())?;
        self.acknowledged_inputs
            .retain(|sequence| *sequence > through);
        Ok(())
    }
    pub fn next_input(&self) -> Result<u64, String> {
        self.number("input_sequence")?
            .checked_add(1)
            .ok_or("Input sequence exhausted".into())
    }
    pub fn acknowledged(&self) -> Result<u64, String> {
        self.number("outcome_ack")
    }
    pub fn latest(&self) -> Result<u64, String> {
        Ok(self.outcome_latest)
    }
    #[cfg(test)]
    pub fn fail_input_storage(&self) {
        self.db.execute_batch("DROP TABLE inputs").unwrap();
    }
    pub fn restart(
        &self,
        id: uuid::Uuid,
        hash: &[u8; 32],
    ) -> Result<Option<super::restart::RestartState>, String> {
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .db
            .query_row(
                "SELECT hash,state FROM restarts WHERE id=?1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        row.map(|(stored, state)| {
            if stored.as_slice() != hash {
                return Err("Restart identity was already used for a different launch".into());
            }
            serde_json::from_slice(&state).map_err(|error| error.to_string())
        })
        .transpose()
    }
    pub fn admit_restart(&mut self, id: uuid::Uuid, hash: &[u8; 32]) -> Result<(), String> {
        let count: usize = self
            .db
            .query_row("SELECT COUNT(*) FROM restarts", [], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if count >= 128 {
            return Err("Retained actor restart receipt capacity reached".into());
        }
        self.db
            .execute(
                "INSERT INTO restarts(id,hash,state) VALUES(?1,?2,?3)",
                params![
                    id.to_string(),
                    hash.as_slice(),
                    serde_json::to_vec(&super::restart::RestartState::Accepted)
                        .map_err(|error| error.to_string())?
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }
    pub fn finish_restart(
        &mut self,
        id: uuid::Uuid,
        result: Result<crate::daemon::holder_registry::Process, super::restart::RestartFailure>,
    ) -> Result<super::restart::RestartState, String> {
        let state = super::restart::RestartState::Acknowledged(result);
        self.db
            .execute(
                "UPDATE restarts SET state=?1 WHERE id=?2",
                params![
                    serde_json::to_vec(&state).map_err(|error| error.to_string())?,
                    id.to_string()
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(state)
    }
    fn number(&self, key: &str) -> Result<u64, String> {
        match key {
            "input_sequence" => Ok(self.input_sequence),
            "outcome_ack" => Ok(self.outcome_ack),
            _ => Err("Invalid actor journal sequence".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn historical_turn(id:&str)->crate::chat::turn_outcomes::TurnIdentity {
        crate::chat::turn_outcomes::TurnIdentity {session_id:"confirmed-session".into(),kind:crate::chat::turn_outcomes::TurnIdentityKind::NativeTurn,id:id.into()}
    }
    #[test]
    fn exact_historical_turn_proof_repeats_until_ack_without_settling_current_run() {
        use crate::chat::turn_outcomes::TurnUpdate;
        let dir=tempfile::tempdir_in("/var/tmp").unwrap();let mut journal=Journal::with_budget(dir.path(),16*1024).unwrap();
        let current=crate::model::RunId::new();let input=InputIdentity {sequence:1,id:uuid::Uuid::new_v4(),hash:[5;32]};
        journal.admit_with_run(&input,Some(current)).unwrap();
        let identity=historical_turn("earlier-turn");
        let updates=[Update::TurnOutcome {update:TurnUpdate::Bind {identity:identity.clone(),item_ids:vec!["actual-provider-item-A".into()]}},
            Update::TurnOutcome {update:TurnUpdate::Finish {identity:identity.clone(),outcome:crate::agent::ActivityOutcome::Cancelled,finished_at:1234}}];
        for update in &updates {journal.append(update).unwrap();}
        let first=journal.page(0).unwrap();let repeated=journal.page(0).unwrap();
        assert_eq!(serde_json::to_value(&first).unwrap(),serde_json::to_value(repeated).unwrap());
        for (entry,expected) in first.iter().zip(&updates) {assert_eq!(serde_json::to_value(&entry.update).unwrap(),serde_json::to_value(expected).unwrap());}
        assert!(matches!(&first[1].update,Update::TurnOutcome {update:TurnUpdate::Finish {identity:at,finished_at:1234,..}} if at==&identity));
        journal.ack(2).unwrap();assert!(journal.page(2).unwrap().is_empty());
        assert_eq!(journal.retained_runs().unwrap(),vec![current],"past proof ACK never commits the current managed run");
        assert_eq!(journal.input(&input).unwrap().unwrap().state,InputState::Accepted);
    }
    #[test]
    fn malformed_turn_proof_is_refused_before_journal_publication() {
        use crate::chat::turn_outcomes::{TurnUpdate,MAX_TURN_ID_BYTES,MAX_TURN_ITEM_IDS};
        let dir=tempfile::tempdir_in("/var/tmp").unwrap();let mut journal=Journal::with_budget(dir.path(),DEFAULT_BUDGET).unwrap();
        let invalid=Update::TurnOutcome {update:TurnUpdate::Bind {identity:historical_turn("old"),item_ids:vec!["duplicate".into(),"duplicate".into()]}};
        assert!(journal.append(&invalid).is_err());assert_eq!(journal.latest().unwrap(),0);
        // Shared TurnUpdate validation caps the aggregate raw proof at 64 KiB;
        // this refusal occurs before encoded-entry capacity/loss accounting.
        let ids=(0..MAX_TURN_ITEM_IDS).map(|index|format!("{index}{}","a".repeat(MAX_TURN_ID_BYTES-4))).collect();
        let large=Update::TurnOutcome {update:TurnUpdate::Bind {identity:historical_turn("old"),item_ids:ids}};
        assert!(journal.append(&large).is_err());
        assert_eq!(journal.latest().unwrap(),0);assert!(journal.continuity_loss().unwrap().is_none());
    }
    #[test]
    fn encoded_semantic_entry_cap_records_a_typed_loss() {
        let dir=tempfile::tempdir_in("/var/tmp").unwrap();let mut journal=Journal::with_budget(dir.path(),DEFAULT_BUDGET).unwrap();
        let large=Update::Attention(crate::notifications::Evidence {
            key:"k".repeat(1024*1024),kind:crate::notifications::Kind::Succeeded,
        });
        assert!(journal.append(&large).unwrap_err().contains("overflowed"));
        assert_eq!(journal.latest().unwrap(),0);assert!(journal.continuity_loss().unwrap().is_some());
    }

    #[test]
    fn immutable_attention_context_repeats_verbatim_until_full_ack() {
        let dir = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal = Journal::with_budget(dir.path(), 16 * 1024).unwrap();
        let evidence = crate::notifications::Evidence {
            key: "exact-provider-turn".into(),
            kind: crate::notifications::Kind::Waiting,
        };
        let content = crate::notifications::Content {
            conversation: "名".repeat(80),
            preview: "😀".repeat(240),
            request: Some(crate::notifications::RequestType::Question),
        };
        let update = Update::AttentionContext {
            evidence: evidence.clone(),
            content: Some(content.clone()),
        };
        journal.append(&update).unwrap();
        let first = journal.page(0).unwrap();
        let second = journal.page(0).unwrap();
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&second).unwrap()
        );
        assert!(
            matches!(&first[0].update,Update::AttentionContext {evidence:at,content:Some(at_content)} if at==&evidence && at_content==&content)
        );
        // The committed bytes themselves retain the immutable payload; this
        // helper never reconstructs its Store or journal during daemon adoption.
        let bytes: Vec<u8> = journal
            .db
            .query_row("SELECT payload FROM outcomes WHERE sequence=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            serde_json::to_value(&update).unwrap()
        );
        journal
            .append(&Update::PastAttention(evidence.clone()))
            .unwrap();
        journal.append(&Update::Presentation).unwrap();
        assert_eq!(
            journal.latest().unwrap(),
            1,
            "old-turn evidence/stream wakes never become current attention"
        );
        journal.ack(1).unwrap();
        assert!(journal.page(1).unwrap().is_empty());
    }

    #[test]
    fn malformed_optional_context_keeps_exact_semantic_evidence_without_later_enrichment() {
        use crate::notifications::{Content, Evidence, Kind, RequestType};
        for content in [
            Content {
                conversation: "x".repeat(81),
                ..Default::default()
            },
            Content {
                preview: "x".repeat(241),
                ..Default::default()
            },
            Content {
                preview: "line\ncontrol".into(),
                ..Default::default()
            },
            Content {
                conversation: "\u{1b}title".into(),
                ..Default::default()
            },
            Content {
                request: Some(RequestType::Approval),
                ..Default::default()
            },
        ] {
            let dir = tempfile::tempdir_in("/var/tmp").unwrap();
            let mut journal = Journal::with_budget(dir.path(), 4096).unwrap();
            let evidence = Evidence {
                key: "known-final-turn".into(),
                kind: Kind::Succeeded,
            };
            journal
                .append(&Update::AttentionContext {
                    evidence: evidence.clone(),
                    content: Some(content),
                })
                .unwrap();
            let page = journal.page(0).unwrap();
            assert!(
                matches!(&page[0].update,Update::AttentionContext {evidence:at,content:None} if at==&evidence)
            );
            assert!(
                journal.continuity_loss().unwrap().is_none(),
                "invalid optional display data does not make known execution uncertain"
            );
            journal
                .append(&Update::Attention(Evidence {
                    key: "legacy-turn".into(),
                    kind: Kind::Cancelled,
                }))
                .unwrap();
            assert_eq!(
                journal.latest().unwrap(),
                2,
                "legacy/new metadata evidence coexist without later Snapshot reads"
            );
        }
    }

    #[test]
    fn restart_identity_is_deduplicated_before_any_replacement_effects() {
        let dir = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal = Journal::with_budget(dir.path(), 4096).unwrap();
        let id = uuid::Uuid::new_v4();
        let hash = [5; 32];
        journal.admit_restart(id, &hash).unwrap();
        assert_eq!(
            journal.restart(id, &hash).unwrap(),
            Some(super::super::restart::RestartState::Accepted)
        );
        assert!(journal.restart(id, &[6; 32]).is_err());
        let process =
            crate::daemon::holder_registry::Process::read(std::process::id() as i32).unwrap();
        journal.finish_restart(id, Ok(process.clone())).unwrap();
        assert_eq!(
            journal.restart(id, &hash).unwrap(),
            Some(super::super::restart::RestartState::Acknowledged(Ok(
                process
            )))
        );
    }
    #[test]
    fn accepted_run_survives_input_ack_and_finished_delivery_until_durable_outcome_ack() {
        let dir = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal = Journal::with_budget(dir.path(), 4096).unwrap();
        let run = crate::model::RunId::new();
        let identity = InputIdentity {
            sequence: 1,
            id: uuid::Uuid::new_v4(),
            hash: [7; 32],
        };
        journal.admit_with_run(&identity, Some(run)).unwrap();
        assert_eq!(journal.retained_runs().unwrap(), vec![run]);
        journal
            .finish(&identity, InputState::Acknowledged(Ok(())))
            .unwrap();
        journal.retire_inputs(1).unwrap();
        journal
            .append(&Update::Execution {
                run,
                update: crate::agent_execution::Update::Started {
                    provider_key: "turn".into(),
                },
            })
            .unwrap();
        journal.ack(1).unwrap();
        assert_eq!(journal.retained_runs().unwrap(), vec![run]);
        journal
            .append(&Update::Execution {
                run,
                update: crate::agent_execution::Update::Finished {
                    state: crate::agent_execution::State::Succeeded,
                    result: Default::default(),
                },
            })
            .unwrap();
        assert_eq!(
            journal.retained_runs().unwrap(),
            vec![run],
            "delivery to a proxy is not a durable registry ACK"
        );
        journal.ack(2).unwrap();
        assert!(journal.retained_runs().unwrap().is_empty());
    }
    #[test]
    fn exact_retirement_survives_more_than_capacity_and_preserves_uncertain_holes() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(directory.path()).unwrap();
        let identity = |sequence| InputIdentity {
            sequence,
            id: uuid::Uuid::new_v4(),
            hash: [7; 32],
        };
        let accepted = identity(1);
        journal.admit(&accepted).unwrap();
        let unknown = identity(2);
        journal.admit(&unknown).unwrap();
        journal
            .finish(
                &unknown,
                InputState::Unknown("write outcome is unknown".into()),
            )
            .unwrap();
        let mut last = None;
        for sequence in 3..(MAX_INPUTS as u64 + 35) {
            let input = identity(sequence);
            journal.admit(&input).unwrap();
            journal
                .finish(&input, InputState::Acknowledged(Ok(())))
                .unwrap();
            let page = journal.acknowledged_input_page().unwrap();
            assert_eq!(page.len(), 1);
            assert_eq!(page[0].identity, input);
            assert!(matches!(&page[0].state, InputState::Acknowledged(Ok(()))));
            assert!(
                journal
                    .retire_acknowledged_inputs(&[input.clone(), unknown.clone()])
                    .is_err()
            );
            assert_eq!(
                journal.acknowledged_input_count(),
                1,
                "failed batch cannot remove an earlier terminal receipt"
            );
            let mut changed = input.clone();
            changed.hash = [8; 32];
            assert!(journal.retire_acknowledged_inputs(&[changed]).is_err());
            assert!(journal.input(&input).unwrap().is_some());
            journal
                .retire_acknowledged_inputs(std::slice::from_ref(&input))
                .unwrap();
            journal
                .retire_acknowledged_inputs(std::slice::from_ref(&input))
                .unwrap();
            assert!(
                journal.admit(&input).is_err(),
                "a retired sequence must never invoke the command again"
            );
            last = Some(input);
        }
        assert!(
            journal
                .retire_acknowledged_inputs(std::slice::from_ref(&accepted))
                .is_err()
        );
        assert!(
            journal
                .retire_acknowledged_inputs(std::slice::from_ref(&unknown))
                .is_err()
        );
        assert_eq!(
            journal.input(&accepted).unwrap().unwrap().state,
            InputState::Accepted
        );
        assert!(matches!(
            journal.input(&unknown).unwrap().unwrap().state,
            InputState::Unknown(_)
        ));
        assert_eq!(journal.acknowledged_input_count(), 0);
        assert!(journal.acknowledged_input_page().unwrap().is_empty());
        let next = journal.next_input().unwrap();
        let retired = last.unwrap();
        drop(journal);
        // Reopening the live actor itself is intentionally forbidden. Inspect
        // its durable high-water tombstone without reconstructing a reducer.
        let database = Connection::open(directory.path().join("actor-journal.sqlite")).unwrap();
        let highwater: String = database
            .query_row(
                "SELECT value FROM metadata WHERE key='input_sequence'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(highwater.parse::<u64>().unwrap() + 1, next);
        let count: u64 = database
            .query_row(
                "SELECT COUNT(*) FROM inputs WHERE sequence=?1",
                [retired.sequence],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        let state: Vec<u8> = database
            .query_row(
                "SELECT state FROM inputs WHERE sequence=?1",
                [accepted.sequence],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<InputState>(&state).unwrap(),
            InputState::Accepted
        );
    }

    #[test]
    fn receipts_never_readmit_duplicate_or_retired_inputs() {
        let dir = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal = Journal::with_budget(dir.path(), 1024).unwrap();
        let identity = InputIdentity {
            sequence: 1,
            id: uuid::Uuid::new_v4(),
            hash: [7; 32],
        };
        assert_eq!(
            journal.admit(&identity).unwrap().state,
            InputState::Accepted
        );
        assert_eq!(
            journal.admit(&identity).unwrap().state,
            InputState::Accepted
        );
        let mut changed = identity.clone();
        changed.hash = [8; 32];
        assert!(journal.admit(&changed).is_err());
        journal
            .finish(&identity, InputState::Acknowledged(Ok(())))
            .unwrap();
        journal.retire_inputs(1).unwrap();
        assert!(journal.admit(&identity).is_err());
        let mut next = identity.clone();
        next.sequence = 3;
        next.id = uuid::Uuid::new_v4();
        assert!(journal.admit(&next).is_err());
        next.sequence = 2;
        assert!(journal.admit(&next).is_ok());
    }
    #[test]
    fn resident_loss_metadata_remains_available_when_the_journal_storage_is_unavailable() {
        let dir = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal = Journal::with_budget(dir.path(), 4096).unwrap();
        let run = crate::model::RunId::new();
        journal
            .admit_with_run(
                &InputIdentity {
                    sequence: 1,
                    id: uuid::Uuid::new_v4(),
                    hash: [7; 32],
                },
                Some(run),
            )
            .unwrap();
        journal
            .db
            .execute_batch("DROP TABLE metadata; DROP TABLE retained_runs;")
            .unwrap();
        assert!(
            journal.record_loss("storage failed").is_err(),
            "disk persistence failure is reported honestly"
        );
        let loss = journal.continuity_loss().unwrap().unwrap();
        assert_eq!(loss.affected_runs, vec![run]);
        assert_eq!(journal.next_input().unwrap(), 2);
        assert_eq!(journal.latest().unwrap(), 0);
        assert_eq!(journal.acknowledged().unwrap(), 0);
        assert_eq!(journal.retained_runs().unwrap(), vec![run]);
        assert!(journal.ack_loss(loss.id).is_err());
        assert!(!journal.continuity_loss().unwrap().unwrap().acknowledged);
    }
    #[test]
    fn typed_loss_requires_exact_durable_ack_and_explicit_restart_reset() {
        let dir = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal = Journal::with_budget(dir.path(), 1).unwrap();
        let run = crate::model::RunId::new();
        let identity = InputIdentity {
            sequence: 1,
            id: uuid::Uuid::new_v4(),
            hash: [3; 32],
        };
        journal.admit_with_run(&identity, Some(run)).unwrap();
        assert!(
            journal
                .append(&Update::Execution {
                    run,
                    update: crate::agent_execution::Update::Started {
                        provider_key: "turn".into()
                    }
                })
                .is_err()
        );
        let loss = journal.continuity_loss().unwrap().unwrap();
        assert_eq!(loss.affected_runs, vec![run]);
        assert!(!loss.acknowledged);
        journal.record_loss("later symptoms").unwrap();
        assert_eq!(journal.continuity_loss().unwrap().unwrap().id, loss.id);
        assert!(journal.clear_acknowledged_loss_for_restart().is_err());
        assert!(journal.ack_loss(uuid::Uuid::new_v4()).is_err());
        journal.ack_loss(loss.id).unwrap();
        assert!(journal.continuity_loss().unwrap().unwrap().acknowledged);
        assert!(
            journal.loss().unwrap().is_some(),
            "host acknowledgement alone never clears a loss or admits input"
        );
        journal.clear_acknowledged_loss_for_restart().unwrap();
        assert!(journal.continuity_loss().unwrap().is_none());
        assert!(journal.retained_runs().unwrap().is_empty());
        assert_eq!(
            journal.next_input().unwrap(),
            2,
            "explicit reset never reuses an accepted input sequence"
        );
    }
    #[test]
    fn overflow_is_durable_and_ack_does_not_hide_the_loss() {
        let dir = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal = Journal::with_budget(dir.path(), 1).unwrap();
        let update = Update::Attention(crate::notifications::Evidence {
            key: "turn".into(),
            kind: crate::notifications::Kind::Succeeded,
        });
        assert!(journal.append(&update).is_err());
        assert!(journal.loss().unwrap().unwrap().contains("overflowed"));
        journal.ack(0).unwrap();
        assert!(journal.loss().unwrap().is_some());
    }
    #[test]
    fn outbox_repeats_until_commit_ack_and_rejects_future_ack() {
        let dir = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal = Journal::with_budget(dir.path(), 4096).unwrap();
        journal
            .append(&Update::Attention(crate::notifications::Evidence {
                key: "turn".into(),
                kind: crate::notifications::Kind::Waiting,
            }))
            .unwrap();
        assert_eq!(journal.page(0).unwrap().len(), 1);
        assert_eq!(journal.page(0).unwrap().len(), 1);
        assert!(journal.ack(2).is_err());
        journal.ack(1).unwrap();
        assert!(journal.page(0).is_err());
        assert!(journal.page(1).unwrap().is_empty());
    }
}
