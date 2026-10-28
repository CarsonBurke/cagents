//! Private semantic outcomes and stable input receipts. The live reducer and
//! transcript remain in the holder; this is never a stdout/reducer replay log.
use super::super::Update;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path};

const DEFAULT_BUDGET: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
pub(super) const MAX_INPUTS: usize = 1024;
const MAX_METADATA_ENTRY_BYTES: usize = 1024 * 1024;
// JSON can encode each source byte as six bytes (for example, \u0000).
// Finished outcomes carry the public result contract in addition to fixed-size
// run/state/usage fields. Keep metadata-only outcomes on their smaller limit.
const MAX_FINISHED_ENTRY_BYTES: usize = 6
    * (crate::agent_execution::MAX_RESULT_BYTES + crate::agent_execution::MAX_DIAGNOSTIC_BYTES)
    + 4096;
const OVERFLOW_REASON: &str = "Claude holder outcome journal overflowed before the daemon committed its updates; the in-flight run outcome is unconfirmed";

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
    retention: crate::transcript_cache::ActorJournalRetention,
    retirement_sealed: bool,
    budget: usize,
    last_loss: Option<ContinuityLoss>,
    run_ids: std::collections::BTreeSet<crate::model::RunId>,
    finished_runs: std::collections::BTreeMap<crate::model::RunId, u64>,
    input_sequence: u64,
    acknowledged_inputs: std::collections::BTreeSet<u64>,
    outcome_ack: u64,
    outcome_latest: u64,
    published_sources: Vec<crate::chat::Session>,
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
        let created = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| format!("Cannot create private actor journal: {error}"))?;
        let retention = crate::transcript_cache::ActorJournalRetention::create(directory, &created)
            .map_err(|error| error.to_string())?;
        let db = Connection::open(path).map_err(|error| error.to_string())?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
            CREATE TABLE outcomes(sequence INTEGER PRIMARY KEY AUTOINCREMENT,payload BLOB NOT NULL);
            CREATE TABLE retained_runs(run TEXT PRIMARY KEY,finished INTEGER);
            CREATE TABLE inputs(sequence INTEGER PRIMARY KEY,id TEXT UNIQUE NOT NULL,hash BLOB NOT NULL,state BLOB NOT NULL);
            CREATE TABLE metadata(key TEXT PRIMARY KEY,value BLOB NOT NULL);
            INSERT INTO metadata VALUES('input_sequence','0'),('outcome_ack','0');")
            .map_err(|error| error.to_string())?;
        Ok(Self {
            db,
            retention,
            retirement_sealed: false,
            budget,
            last_loss: None,
            run_ids: Default::default(),
            finished_runs: Default::default(),
            input_sequence: 0,
            acknowledged_inputs: Default::default(),
            outcome_ack: 0,
            outcome_latest: 0,
            published_sources: Vec::new(),
        })
    }
    /// Exact descriptors issued by this live helper. This is not a provider
    /// replay/adoption log: each helper creates its own journal exclusively.
    pub fn remember_published_source(
        &mut self,
        source: &crate::chat::Session,
    ) -> Result<(), String> {
        // Known aliases remain available after sealing, with no transaction or
        // fsync on the repeated publication/inspection path.
        if self.published_sources.contains(source) {
            return Ok(());
        }
        self.ensure_writable()?;
        if self.published_sources.len() >= 1024 {
            return Err("Retained source lineage capacity reached".into());
        }
        let mut sources = self.published_sources.clone();
        sources.push(source.clone());
        let bytes = serde_json::to_vec(&sources).map_err(|error| error.to_string())?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err("Retained source lineage capacity reached".into());
        }
        // synchronous=FULL is established at journal creation. Publish the
        // in-memory view only after the exact aggregate commits successfully.
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        tx.execute(
            "INSERT INTO metadata(key,value) VALUES('published_sources',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [bytes],
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        self.published_sources = sources;
        Ok(())
    }

    pub fn published_sources(&self) -> &[crate::chat::Session] {
        &self.published_sources
    }

    pub fn continuity_loss(&self) -> Result<Option<ContinuityLoss>, String> {
        Ok(self.last_loss.clone())
    }
    pub fn loss(&self) -> Result<Option<String>, String> {
        Ok(self.continuity_loss()?.map(|loss| loss.reason))
    }
    pub fn record_loss(&mut self, reason: &str) -> Result<(), String> {
        self.ensure_writable()?;
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
        self.ensure_writable()?;
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
        self.ensure_writable()?;
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
            Update::Execution { .. } | Update::Attention(_) | Update::AttentionContext { .. }
        ) {
            return Ok(());
        }
        if self.retirement_sealed {
            return Err("Retained semantic journal is sealed for final retirement".into());
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
        let entry_limit = match update {
            Update::Execution {
                update: crate::agent_execution::Update::Finished { result, .. },
                ..
            } => {
                if result.text.len() > crate::agent_execution::MAX_RESULT_BYTES
                    || result.diagnostic.as_ref().is_some_and(|diagnostic| {
                        diagnostic.len() > crate::agent_execution::MAX_DIAGNOSTIC_BYTES
                    })
                {
                    self.record_loss(OVERFLOW_REASON)?;
                    return Err(OVERFLOW_REASON.into());
                }
                MAX_FINISHED_ENTRY_BYTES
            }
            _ => MAX_METADATA_ENTRY_BYTES,
        };
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
            || bytes.len() > entry_limit
            || retained.saturating_add(bytes.len()) > self.budget
        {
            self.record_loss(OVERFLOW_REASON)?;
            return Err(OVERFLOW_REASON.into());
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
        self.ensure_writable()?;
        let previous = self.number("outcome_ack")?;
        if through <= previous {
            // Independent guarded HOST drainers may commit overlapping pages.
            // A stale ACK is idempotent; it never rewinds the durable cursor.
            return Ok(());
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
        self.ensure_writable()?;
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
        self.ensure_writable()?;
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
        self.ensure_writable()?;
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
        self.ensure_writable()?;
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
        self.ensure_writable()?;
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

    fn ensure_writable(&self) -> Result<(), String> {
        if self.retirement_sealed {
            Err("Retained semantic journal is sealed for final retirement".into())
        } else {
            Ok(())
        }
    }

    /// Called with the journal mutex held after the provider has joined and
    /// final input completions have drained. No later semantic append is valid.
    pub fn seal_for_retirement(&mut self) -> Result<(), String> {
        // Retirement is infrequent and already outside the live reducer.
        // Confirm the actual durable queue is empty as well as its cached
        // clocks; storage faults may make a cache an insufficient disposition.
        let queued: u64 = self
            .db
            .query_row("SELECT COUNT(*) FROM outcomes", [], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if queued != 0
            || self.acknowledged()? != self.latest()?
            || self
                .last_loss
                .as_ref()
                .is_some_and(|loss| !loss.acknowledged)
        {
            return Err("Unacknowledged semantic evidence cannot become disposable cache".into());
        }
        self.retirement_sealed = true;
        Ok(())
    }

    pub fn allow_retired_cache_removal(&self) -> Result<(), String> {
        if !self.retirement_sealed {
            return Err("Retained semantic journal has not been sealed".into());
        }
        self.retention
            .allow_removal()
            .map_err(|error| error.to_string())
    }
    #[cfg(test)]
    pub fn fail_input_storage(&self) {
        self.db.execute_batch("DROP TABLE inputs").unwrap();
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
    fn durable_sources(directory: &Path) -> Vec<crate::chat::Session> {
        let db = Connection::open_with_flags(
            directory.join("actor-journal.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let bytes: Option<Vec<u8>> = db
            .query_row(
                "SELECT value FROM metadata WHERE key='published_sources'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        bytes
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
            .unwrap_or_default()
    }

    #[test]
    fn published_sources_preserve_exact_pre_id_and_thread_aliases_durably() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(root.path()).unwrap();
        let pre_id = super::super::transport::fixture_source();
        let mut a = pre_id.clone();
        a.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        let mut b = a.clone();
        b.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        for source in [&pre_id, &a, &b] {
            journal.remember_published_source(source).unwrap();
        }
        assert_eq!(
            journal.published_sources(),
            &[pre_id.clone(), a.clone(), b.clone()]
        );
        assert_eq!(durable_sources(root.path()), journal.published_sources());
        journal.db.execute_batch(
            "CREATE TRIGGER no_publication_insert BEFORE INSERT ON metadata WHEN NEW.key='published_sources' BEGIN SELECT RAISE(ABORT,'unexpected publication write'); END;
             CREATE TRIGGER no_publication_update BEFORE UPDATE ON metadata WHEN NEW.key='published_sources' BEGIN SELECT RAISE(ABORT,'unexpected publication write'); END;",
        ).unwrap();
        // Neither repeated publication nor a known post-seal lookup may write.
        journal.remember_published_source(&a).unwrap();
        journal.seal_for_retirement().unwrap();
        journal.remember_published_source(&b).unwrap();
        let mut unknown = b;
        unknown.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(journal.remember_published_source(&unknown).is_err());
        assert_eq!(durable_sources(root.path()), journal.published_sources());
    }

    #[test]
    fn failed_publication_commit_leaves_cache_and_durable_aliases_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(root.path()).unwrap();
        let source = super::super::transport::fixture_source();
        journal.remember_published_source(&source).unwrap();
        // The metadata statement succeeds, but its deferred constraint fails
        // at COMMIT, exercising rollback rather than an admission-only error.
        journal.db.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE publication_parent(id INTEGER PRIMARY KEY);
             CREATE TABLE publication_fault(parent INTEGER REFERENCES publication_parent(id) DEFERRABLE INITIALLY DEFERRED);
             CREATE TRIGGER fail_publication_commit AFTER UPDATE ON metadata WHEN NEW.key='published_sources' BEGIN INSERT INTO publication_fault VALUES(1); END;",
        ).unwrap();
        let mut next = source.clone();
        next.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(journal.remember_published_source(&next).is_err());
        assert_eq!(journal.published_sources(), &[source]);
        assert_eq!(durable_sources(root.path()), journal.published_sources());
        journal
            .db
            .execute_batch("DROP TRIGGER fail_publication_commit;")
            .unwrap();
        journal.remember_published_source(&next).unwrap();
        assert_eq!(durable_sources(root.path()), journal.published_sources());
    }

    #[test]
    fn published_source_count_and_aggregate_byte_bounds_preserve_prior_aliases() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(root.path()).unwrap();
        let source = super::super::transport::fixture_source();
        for index in 0..1024 {
            let mut alias = source.clone();
            alias.launch.session_id = Some(uuid::Uuid::from_u128(index + 1).to_string());
            journal.remember_published_source(&alias).unwrap();
        }
        let before = journal.published_sources().to_vec();
        assert!(journal.remember_published_source(&source).is_err());
        assert_eq!(journal.published_sources(), before);
        assert_eq!(durable_sources(root.path()), before);

        let byte_root = tempfile::tempdir().unwrap();
        let mut bytes = Journal::open(byte_root.path()).unwrap();
        let mut large = source.clone();
        large
            .launch
            .recipe
            .arguments
            .push("x".repeat(2 * 1024 * 1024));
        bytes.remember_published_source(&large).unwrap();
        let mut next_large = large.clone();
        next_large.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(serde_json::to_vec(&[&next_large]).unwrap().len() < 4 * 1024 * 1024);
        assert!(bytes.remember_published_source(&next_large).is_err());
        assert_eq!(bytes.published_sources(), &[large]);
        assert_eq!(durable_sources(byte_root.path()), bytes.published_sources());
        bytes.seal_for_retirement().unwrap();
    }

    #[test]
    fn unacknowledged_sqlite_journal_survives_actual_store_drop() {
        let store = super::super::super::Store::new().unwrap();
        let directory = store.directory().to_owned();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        let mut journal = Journal::open(&directory).unwrap();
        journal
            .append(&Update::Attention(crate::notifications::Evidence {
                key: "retained-after-drop".into(),
                kind: crate::notifications::Kind::Succeeded,
            }))
            .unwrap();
        assert!(journal.seal_for_retirement().is_err());
        assert!(journal.allow_retired_cache_removal().is_err());
        drop(store);
        assert!(directory.join("actor-journal.sqlite").exists());
        assert_eq!(journal.page(0).unwrap().len(), 1);
        assert!(!crate::transcript_cache::remove_if_reclaimable(&directory).unwrap());
    }

    #[test]
    fn cache_clocks_cannot_hide_a_durable_semantic_row_from_retirement() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(root.path()).unwrap();
        let update = Update::Attention(crate::notifications::Evidence {
            key: "durable-row-without-cache-publication".into(),
            kind: crate::notifications::Kind::Succeeded,
        });
        // Deterministically model durable commit/cache-publication divergence;
        // this is not an asserted reproduction of a particular SQLite failure.
        journal
            .db
            .execute(
                "INSERT INTO outcomes(payload) VALUES(?1)",
                [serde_json::to_vec(&update).unwrap()],
            )
            .unwrap();
        assert_eq!(journal.acknowledged().unwrap(), journal.latest().unwrap());
        assert!(journal.seal_for_retirement().is_err());
        assert!(journal.allow_retired_cache_removal().is_err());
    }

    #[test]
    fn checked_retirement_fences_mutation_and_allows_delayed_reader_store_drop() {
        let store = std::sync::Arc::new(super::super::super::Store::new().unwrap());
        let reader = store.clone();
        let directory = store.directory().to_owned();
        let mut journal = Journal::open(&directory).unwrap();
        let update = Update::Attention(crate::notifications::Evidence {
            key: "durably-committed".into(),
            kind: crate::notifications::Kind::Succeeded,
        });
        journal.append(&update).unwrap();
        journal.ack(1).unwrap();
        journal
            .record_loss("explicitly committed continuity loss")
            .unwrap();
        assert!(journal.seal_for_retirement().is_err());
        let loss = journal.continuity_loss().unwrap().unwrap();
        journal.ack_loss(loss.id).unwrap();
        journal.seal_for_retirement().unwrap();
        assert!(journal.append(&update).is_err());
        assert!(journal.record_loss("late mutation").is_err());
        journal.allow_retired_cache_removal().unwrap();
        drop(store);
        assert!(directory.exists(), "reader retains the actual Store lease");
        drop(journal);
        drop(reader);
        assert!(!directory.exists());
    }

    #[test]
    fn encoded_semantic_entry_cap_records_a_typed_loss() {
        let dir = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal = Journal::with_budget(dir.path(), DEFAULT_BUDGET).unwrap();
        let large = Update::Attention(crate::notifications::Evidence {
            key: "k".repeat(1024 * 1024),
            kind: crate::notifications::Kind::Succeeded,
        });
        assert!(journal.append(&large).unwrap_err().contains("overflowed"));
        assert_eq!(journal.latest().unwrap(), 0);
        assert!(journal.continuity_loss().unwrap().is_some());
    }

    #[test]
    fn full_size_finished_results_repeat_exactly_until_durable_ack() {
        use crate::agent_execution::{
            MAX_DIAGNOSTIC_BYTES, MAX_RESULT_BYTES, ResultSummary, State,
        };
        for text in [
            "r".repeat(MAX_RESULT_BYTES),
            "\0".repeat(MAX_RESULT_BYTES),
            "😀".repeat(MAX_RESULT_BYTES / 4),
        ] {
            let dir = tempfile::tempdir_in("/var/tmp").unwrap();
            let mut journal = Journal::open(dir.path()).unwrap();
            let run = crate::model::RunId::new();
            let update = Update::Execution {
                run,
                update: crate::agent_execution::Update::Finished {
                    state: State::Succeeded,
                    result: ResultSummary {
                        text,
                        diagnostic: Some("\u{1}".repeat(MAX_DIAGNOSTIC_BYTES)),
                        usage: crate::chat::Usage {
                            input_tokens: u64::MAX,
                            output_tokens: u64::MAX,
                            context_tokens: u64::MAX,
                            context_limit: u64::MAX,
                            cost_microusd: Some(u64::MAX),
                            cost_estimate: Some(crate::chat::pricing::CostEstimate {
                                microusd: u64::MAX,
                                partial: true,
                            }),
                        },
                        ..Default::default()
                    },
                },
            };
            let encoded = serde_json::to_vec(&update).unwrap();
            assert!(encoded.len() > MAX_METADATA_ENTRY_BYTES);
            assert!(encoded.len() <= MAX_FINISHED_ENTRY_BYTES);
            assert!(MAX_FINISHED_ENTRY_BYTES + 4096 < super::super::MAX_FRAME);
            journal.append(&update).unwrap();
            let following = Update::Attention(crate::notifications::Evidence {
                key: "following-outcome".into(),
                kind: crate::notifications::Kind::Succeeded,
            });
            journal.append(&following).unwrap();
            for _ in 0..2 {
                let page = journal.page(0).unwrap();
                assert_eq!(
                    page.len(),
                    1,
                    "one large entry must still make page progress"
                );
                assert_eq!(page[0].sequence, 1);
                assert_eq!(page[0].update, update);
                let response: Result<serde_json::Value, String> =
                    Ok(serde_json::to_value(&page).unwrap());
                let wire = serde_json::to_vec(&response).unwrap();
                assert!(wire.len() < super::super::MAX_FRAME);
                let response: Result<serde_json::Value, String> =
                    serde_json::from_slice(&wire).unwrap();
                let received: Vec<OutboxEntry> = serde_json::from_value(response.unwrap()).unwrap();
                assert_eq!(received[0].update, update);
                assert_eq!(journal.acknowledged().unwrap(), 0);
                assert_eq!(journal.retained_runs().unwrap(), vec![run]);
                assert!(journal.seal_for_retirement().is_err());
            }
            assert!(journal.ack(3).is_err());
            journal.ack(1).unwrap();
            assert!(journal.retained_runs().unwrap().is_empty());
            let page = journal.page(1).unwrap();
            assert_eq!(page.len(), 1);
            assert_eq!(page[0].sequence, 2);
            assert_eq!(page[0].update, following);
            journal.ack(2).unwrap();
            assert!(journal.page(2).unwrap().is_empty());
            assert!(journal.continuity_loss().unwrap().is_none());
            journal.seal_for_retirement().unwrap();
        }
    }

    #[test]
    fn oversized_finished_fields_and_total_encoded_budget_remain_fail_closed() {
        use crate::agent_execution::{
            MAX_DIAGNOSTIC_BYTES, MAX_RESULT_BYTES, ResultSummary, State,
        };
        for (text_bytes, diagnostic_bytes, budget) in [
            (MAX_RESULT_BYTES + 1, 0, DEFAULT_BUDGET),
            (0, MAX_DIAGNOSTIC_BYTES + 1, DEFAULT_BUDGET),
            (
                MAX_RESULT_BYTES,
                MAX_DIAGNOSTIC_BYTES,
                MAX_METADATA_ENTRY_BYTES,
            ),
        ] {
            let dir = tempfile::tempdir_in("/var/tmp").unwrap();
            let mut journal = Journal::with_budget(dir.path(), budget).unwrap();
            let run = crate::model::RunId::new();
            let update = Update::Execution {
                run,
                update: crate::agent_execution::Update::Finished {
                    state: State::Succeeded,
                    result: ResultSummary {
                        text: "\0".repeat(text_bytes),
                        diagnostic: Some("\u{1}".repeat(diagnostic_bytes)),
                        ..Default::default()
                    },
                },
            };
            assert!(journal.append(&update).unwrap_err().contains("overflowed"));
            assert_eq!(journal.latest().unwrap(), 0);
            assert!(journal.page(0).unwrap().is_empty());
            let loss = journal.continuity_loss().unwrap().unwrap();
            assert_eq!(loss.affected_runs, vec![run]);
            journal.ack(0).unwrap();
            assert_eq!(journal.continuity_loss().unwrap().unwrap(), loss);
            assert!(journal.seal_for_retirement().is_err());
        }
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
        journal.append(&Update::PromptAvailable).unwrap();
        journal.append(&Update::Presentation).unwrap();
        assert_eq!(
            journal.latest().unwrap(),
            1,
            "prompt-availability and presentation wakes never enter the semantic outbox"
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
        journal
            .append(&Update::Attention(crate::notifications::Evidence {
                key: "next-turn".into(),
                kind: crate::notifications::Kind::Succeeded,
            }))
            .unwrap();
        journal.ack(2).unwrap();
        // A broker can finish a newer overlapping page before the synchronous
        // Answer drainer acknowledges its captured, already committed page.
        journal.ack(1).unwrap();
        journal.ack(0).unwrap();
        assert_eq!(journal.acknowledged().unwrap(), 2);
        drop(journal);
        // Journals are exclusively created, never reopened for replay. Verify
        // the committed cursor and deletion with an independent read-only DB.
        let persisted = Connection::open_with_flags(
            dir.path().join("actor-journal.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let acknowledged: String = persisted
            .query_row(
                "SELECT value FROM metadata WHERE key='outcome_ack'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(acknowledged, "2");
        let queued: u64 = persisted
            .query_row("SELECT COUNT(*) FROM outcomes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(queued, 0);
    }
}
