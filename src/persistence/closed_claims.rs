//! Small close authority independent of mutable workspace snapshots and archives.
use super::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::os::unix::ffi::OsStrExt;

#[cfg(test)]
thread_local! {
    static PLAIN_FINGERPRINT_EVALUATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum RecoveryContent {
    Conversation(crate::agent::Provider, uuid::Uuid),
    Plain([u8; 32]),
}

impl RecoveryContent {
    fn of(session: &TerminalSession) -> Option<Self> {
        let direct = crate::upgrade::exact_conversation(&session.launch);
        let resumed = session
            .agent_resume_recipe()
            .as_ref()
            .and_then(crate::upgrade::exact_conversation);
        if direct.is_some() && resumed.is_some() && direct != resumed {
            return None;
        }
        if let Some((provider, id)) = direct.or(resumed) {
            return Some(Self::Conversation(provider, id));
        }
        if session.resume_hint.is_some()
            || crate::agent::Provider::from_program(&session.launch.program).is_some()
        {
            return None;
        }
        let crate::model::LaunchRecipe {
            program,
            arguments,
            working_directory,
            environment,
            unset_environment,
        } = &session.launch;
        #[cfg(test)]
        PLAIN_FINGERPRINT_EVALUATIONS.with(|count| count.set(count.get() + 1));
        let mut digest = Sha256::new();
        fn field(digest: &mut Sha256, bytes: &[u8]) {
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        }
        field(&mut digest, program.as_bytes());
        digest.update((arguments.len() as u64).to_le_bytes());
        for argument in arguments {
            field(&mut digest, argument.as_bytes());
        }
        field(&mut digest, working_directory.as_os_str().as_bytes());
        digest.update((environment.len() as u64).to_le_bytes());
        for (key, value) in environment {
            field(&mut digest, key.as_bytes());
            field(&mut digest, value.as_bytes());
        }
        digest.update((unset_environment.len() as u64).to_le_bytes());
        for name in unset_environment {
            field(&mut digest, name.as_bytes());
        }
        Some(Self::Plain(digest.finalize().into()))
    }
}

/// IDs, exact host proof and a bounded recovery-content digest; no launch
/// arguments, environment, transcript, draft or rendered text remain resident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ClosedTerminalClaim {
    id: TerminalId,
    proof: Option<HostSessionProof>,
    origin: Option<TerminalId>,
    content: Option<RecoveryContent>,
}

impl ClosedTerminalClaim {
    #[cfg(test)]
    pub(crate) fn plain_fingerprint_evaluations_for_test() -> usize {
        PLAIN_FINGERPRINT_EVALUATIONS.with(std::cell::Cell::get)
    }

    pub(crate) fn of(session: &TerminalSession) -> Self {
        Self {
            id: session.id,
            proof: session.continuity.live_proof(),
            origin: session.recovery_origin,
            content: RecoveryContent::of(session),
        }
    }
    fn identity(id: TerminalId) -> Self {
        Self {
            id,
            proof: None,
            origin: None,
            content: None,
        }
    }
    pub(crate) fn claims(&self, hosted: &TerminalSession) -> bool {
        self.id == hosted.id
            || (self.proof.is_some()
                && self.proof != hosted.continuity.live_proof()
                && hosted.recovery_origin == Some(self.origin.unwrap_or(self.id))
                && self
                    .content
                    .as_ref()
                    .is_some_and(|content| RecoveryContent::of(hosted).as_ref() == Some(content)))
    }
}

/// Only a requested identity crosses the existing ordered writer lane. This
/// receipt owns no executable, environment, transcript or presentation body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClosureQuery {
    Card(CardId),
    Terminal(ClosedTerminalClaim),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ClosureKey {
    Card(CardId),
    Terminal(TerminalId),
}

impl ClosureQuery {
    pub(crate) fn terminal(session: &TerminalSession) -> Self {
        // With no recovery root, only the exact terminal ID can be claimed.
        // Do not hash its potentially large launch arguments for that lookup.
        Self::Terminal(if session.recovery_origin.is_none() {
            ClosedTerminalClaim::identity(session.id)
        } else {
            ClosedTerminalClaim::of(session)
        })
    }
    pub(crate) fn key(&self) -> ClosureKey {
        match self {
            Self::Card(id) => ClosureKey::Card(*id),
            Self::Terminal(receipt) => ClosureKey::Terminal(receipt.id),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct ClosureStamp {
    sequence: i64,
    unknown: bool,
}
impl ClosureStamp {
    fn read(connection: &Connection) -> Result<Self> {
        let sequence = connection
            .prepare_cached("SELECT COALESCE(MAX(sequence), 0) FROM closed_cards")?
            .query_row([], |row| row.get(0))?;
        let unknown = connection
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM closed_terminal_claims WHERE unknown = 1)",
            )?
            .query_row([], |row| row.get(0))?;
        Ok(Self { sequence, unknown })
    }
}

pub(super) struct ClosedAuthority {
    stamp: ClosureStamp,
    // Contains answers only for identities actually requested in this process.
    // A new closure discards answers before its transaction can commit.
    answers: HashMap<ClosureKey, (ClosureQuery, std::result::Result<bool, ()>)>,
    changed: bool,
    pending_commit: bool,
}
impl ClosedAuthority {
    pub(super) fn open(connection: &Connection) -> Result<Self> {
        Ok(Self {
            stamp: ClosureStamp::read(connection)?,
            answers: HashMap::new(),
            changed: false,
            pending_commit: false,
        })
    }
    fn lookup(&self, query: &ClosureQuery) -> Option<std::result::Result<bool, ()>> {
        if matches!(query, ClosureQuery::Terminal(_)) && self.stamp.unknown {
            return Some(Err(()));
        }
        if self.pending_commit {
            return None;
        }
        if let Some((receipt, closed)) = self.answers.get(&query.key()) {
            if receipt == query {
                return Some(*closed);
            }
        }
        (self.stamp.sequence == 0).then_some(Ok(false))
    }
}

impl StateStore {
    /// Called while the Store connection is held, before COMMIT. Clear prior
    /// answers without holding this RAM lock across SQLite work or fsync.
    pub(super) fn prepare_closure_commit(
        &self,
        transaction: &Transaction<'_>,
    ) -> Result<ClosureStamp> {
        let stamp = ClosureStamp::read(transaction)?;
        let mut authority = self
            .closed_authority
            .write()
            .map_err(|_| StoreError::MutexPoisoned)?;
        if stamp.sequence != authority.stamp.sequence || (stamp.unknown && !authority.stamp.unknown)
        {
            authority.answers.clear();
            // Never publish an uncommitted sequence: a rolled-back sequence
            // may later be reused by the actual next closure.
            authority.pending_commit = true;
            authority.changed = true;
        }
        Ok(stamp)
    }
    pub(super) fn finish_closure_commit(&self, stamp: ClosureStamp) -> Result<()> {
        let mut authority = self
            .closed_authority
            .write()
            .map_err(|_| StoreError::MutexPoisoned)?;
        authority.stamp.sequence = stamp.sequence;
        authority.stamp.unknown |= stamp.unknown;
        authority.pending_commit = false;
        Ok(())
    }
    /// Metadata-only current observation. None is pending/refused, never a
    /// certificate that this identity was not previously closed.
    pub(crate) fn cached_closure(&self, query: &ClosureQuery) -> Option<bool> {
        self.cached_closure_result(query)
            .and_then(std::result::Result::ok)
    }
    /// Outer None requests a worker lookup. Inner Err is a cached refusal,
    /// never evidence that this card was closed and never an automatic retry.
    pub(crate) fn cached_closure_result(
        &self,
        query: &ClosureQuery,
    ) -> Option<std::result::Result<bool, ()>> {
        match self.closed_authority.read() {
            Ok(authority) => authority.lookup(query),
            Err(_) => Some(Err(())),
        }
    }
    pub(crate) fn take_closure_updates(&self) -> bool {
        self.closed_authority
            .write()
            .is_ok_and(|mut authority| std::mem::take(&mut authority.changed))
    }
    /// Runs on the ordered persistence worker. Retain the actual connection
    /// mutex through answer publication, so a newer close cannot be undone by
    /// a delayed pre-close negative result.
    pub(crate) fn resolve_closure(&self, query: ClosureQuery) -> Result<bool> {
        let inner = match self.lock_inner() {
            Ok(inner) => inner,
            Err(error) => {
                if let Ok(mut authority) = self.closed_authority.write() {
                    authority.stamp.unknown = true;
                    authority.answers.insert(query.key(), (query, Err(())));
                    authority.changed = true;
                }
                return Err(error);
            }
        };
        let mut actual_stamp = None;
        let result = ensure_open(&inner).and_then(|_| {
            let stamp = ClosureStamp::read(&inner.connection)?;
            actual_stamp = Some(stamp);
            if stamp.unknown && matches!(query, ClosureQuery::Terminal(_)) {
                return Err(StoreError::InvalidState(
                    "closed terminal attribution is unavailable".into(),
                ));
            }
            self.query_closure(&inner.connection, &query)
        });
        let mut authority = self
            .closed_authority
            .write()
            .map_err(|_| StoreError::MutexPoisoned)?;
        if let Some(stamp) = actual_stamp {
            if stamp.sequence != authority.stamp.sequence
                || (stamp.unknown && !authority.stamp.unknown)
            {
                authority.answers.clear();
            }
            authority.stamp.sequence = stamp.sequence;
            authority.stamp.unknown |= stamp.unknown;
        }
        authority.pending_commit = false;
        if result.is_err() {
            authority.stamp.unknown = true;
        }
        authority.answers.insert(
            query.key(),
            (query, result.as_ref().copied().map_err(|_| ())),
        );
        authority.changed = true;
        result
    }
    fn query_closure(&self, connection: &Connection, query: &ClosureQuery) -> Result<bool> {
        match query {
            ClosureQuery::Card(id) => Ok(connection
                .prepare_cached("SELECT EXISTS(SELECT 1 FROM closed_cards WHERE card_id=?1)")?
                .query_row([id.to_string()], |row| row.get(0))?),
            ClosureQuery::Terminal(receipt) => {
                if self.has_unknown_closed_claims() {
                    return Err(StoreError::InvalidState(
                        "closed terminal attribution is unavailable".into(),
                    ));
                }
                if connection.prepare_cached("SELECT EXISTS(SELECT 1 FROM closed_terminal_claim_keys WHERE terminal_id=?1)")?
                    .query_row([receipt.id.to_string()], |row| row.get::<_, bool>(0))? { return Ok(true); }
                let Some(root) = receipt.origin else {
                    return Ok(false);
                };
                let mut statement = connection.prepare_cached(
                    "SELECT claim_json FROM closed_terminal_claim_keys WHERE lineage_root=?1",
                )?;
                let rows =
                    statement.query_map([root.to_string()], |row| row.get::<_, String>(0))?;
                for row in rows {
                    let claim: ClosedTerminalClaim = from_json("closed terminal claim", &row?)?;
                    if claim.proof.is_some()
                        && claim.proof != receipt.proof
                        && receipt.origin == Some(claim.origin.unwrap_or(claim.id))
                        && claim
                            .content
                            .as_ref()
                            .is_some_and(|content| receipt.content.as_ref() == Some(content))
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }
}

/// Called once at migration and once per newly committed tombstone, never for
/// an unchanged closed snapshot. The compact descriptor survives compaction.
pub(super) fn save_claims(transaction: &Transaction<'_>, id: CardId) -> Result<()> {
    let kind: Option<String> = transaction
        .query_row(
            "SELECT kind_tag FROM cards WHERE card_id = ?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let claims = match kind.as_deref() {
        Some("terminal") => {
            let session = closed_terminal_source(transaction, id)?
                .ok_or_else(|| StoreError::InvalidState("closed terminal disappeared".into()))?;
            let CardKind::Terminal(session) = session.kind else {
                unreachable!()
            };
            vec![ClosedTerminalClaim::of(&session)]
        }
        Some("chat") => {
            let json: String = transaction.query_row(
                "SELECT content_json FROM cards WHERE card_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )?;
            let session: crate::chat::Session = from_json("closed chat claim", &json)?;
            let mut claims = vec![ClosedTerminalClaim::identity(session.id)];
            if let Some(terminal) = &session.terminal {
                claims.push(ClosedTerminalClaim::of(terminal));
            }
            claims
        }
        None => {
            return Err(StoreError::InvalidState(
                "closed card content never reached storage; resource attribution is unknown".into(),
            ));
        }
        _ => Vec::new(),
    };
    if !claims.is_empty() {
        transaction.execute("INSERT INTO closed_terminal_claims(card_id, claims_json) VALUES (?1, ?2) ON CONFLICT(card_id) DO NOTHING", params![id.to_string(), to_json(&claims)?])?;
        for (ordinal, claim) in claims.iter().enumerate() {
            let root = (claim.proof.is_some() && claim.content.is_some())
                .then(|| claim.origin.unwrap_or(claim.id).to_string());
            transaction.execute("INSERT INTO closed_terminal_claim_keys(card_id, ordinal, terminal_id, lineage_root, claim_json) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(card_id,ordinal) DO NOTHING", params![id.to_string(), ordinal as i64, claim.id.to_string(), root, to_json(claim)?])?;
        }
    }
    Ok(())
}

pub(super) fn preserve_unknown_claim(transaction: &Transaction<'_>, id: CardId) -> Result<()> {
    transaction.execute("INSERT INTO closed_terminal_claims(card_id, claims_json, unknown) VALUES (?1, '[]', 1) ON CONFLICT(card_id) DO UPDATE SET unknown = 1", [id.to_string()])?;
    Ok(())
}

impl StateStore {
    /// Canonical indexed check for worker/storage callers. UI callers use the
    /// SessionRuntime cache and enqueue the same request instead of SQLite I/O.
    pub fn card_was_closed(&self, id: CardId) -> bool {
        self.resolve_closure(ClosureQuery::Card(id)).unwrap_or(true)
    }
    pub fn has_unknown_closed_claims(&self) -> bool {
        self.closed_authority
            .read()
            .map_or(true, |authority| authority.stamp.unknown)
    }
    pub fn terminal_was_closed(&self, hosted: &TerminalSession) -> bool {
        self.resolve_closure(ClosureQuery::terminal(hosted))
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HostIncarnation, HostSessionId, LaunchRecipe};

    fn plain() -> TerminalSession {
        TerminalSession {
            id: TerminalId::new(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: vec!["-c".into(), "x".repeat(512 * 1024)],
                working_directory: "/project".into(),
                environment: BTreeMap::new(),
                unset_environment: BTreeSet::new(),
            },
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::Live {
                host_session_id: HostSessionId::new("old"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("old"),
            },
        }
    }

    #[test]
    fn compact_claim_keeps_exact_lineage_without_retaining_the_launch_payload() {
        let saved = plain();
        let claim = ClosedTerminalClaim::of(&saved);
        assert!(serde_json::to_vec(&claim).unwrap().len() < 1024);
        let mut hosted = saved.clone();
        hosted.id = TerminalId::new();
        hosted.recovery_origin = Some(saved.id);
        // A new ID without a changed issued host proof is no recovery.
        assert!(!claim.claims(&hosted));
        hosted.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("new"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("new"),
        };
        assert!(claim.claims(&hosted));
        hosted.launch.unset_environment.insert("PROFILE".into());
        assert!(!claim.claims(&hosted));
        hosted.launch.unset_environment.clear();
        hosted
            .launch
            .environment
            .insert("PROFILE".into(), "other".into());
        assert!(!claim.claims(&hosted));
        hosted.launch.environment.clear();
        hosted.recovery_origin = Some(TerminalId::new());
        assert!(!claim.claims(&hosted));
    }

    #[test]
    fn native_claim_matches_only_the_exact_unambiguous_resumed_conversation() {
        let mut saved = plain();
        saved.launch.program = "omp".into();
        saved.launch.arguments = vec![
            "--resume".into(),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        ];
        let claim = ClosedTerminalClaim::of(&saved);
        let mut hosted = saved.clone();
        hosted.id = TerminalId::new();
        hosted.predecessor = Some(saved.id);
        hosted.recovery_origin = Some(saved.id);
        hosted.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("new"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("new"),
        };
        assert!(claim.claims(&hosted));
        hosted.launch.arguments[1] = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into();
        assert!(!claim.claims(&hosted));
        // Direct ID ownership remains authoritative even if the daemon's
        // descriptor changed content: it cannot reopen a locally closed ID.
        hosted.id = saved.id;
        assert!(claim.claims(&hosted));
    }
}
