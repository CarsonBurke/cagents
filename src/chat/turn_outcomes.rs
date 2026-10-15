//! Terminal results have their own durable identity. A result for an earlier
//! turn never borrows the identity of the currently running turn.
use crate::agent::ActivityOutcome;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_TURN_ID_BYTES: usize = 4096;
pub const MAX_TURN_ITEM_IDS: usize = 128;
pub const MAX_TURN_REASON_BYTES: usize = 4096;
pub const MAX_TURN_UPDATE_BYTES: usize = 64 * 1024;
pub const MAX_PROJECTED_TURNS: usize = 128;
pub const MAX_PROJECTION_BYTES: usize = 256 * 1024;

/// Physical source of parsed native history, not a selected account or login.
/// The producer captures this beside the opened history file; consumers must
/// revalidate it and keep it separate from live/original-command authority.
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct HistoryAccountOrigin {
    pub provider:crate::agent::Provider,
    pub canonical_home:std::path::PathBuf,
    pub device:u64,
    pub inode:u64,
}
impl HistoryAccountOrigin {
    pub fn validate(&self)->Result<(),String> {
        use std::os::unix::ffi::OsStrExt;
        if !matches!(self.provider,crate::agent::Provider::Claude|crate::agent::Provider::Codex)
            || !self.canonical_home.is_absolute() || self.canonical_home.as_os_str().as_bytes().len()>4096
            || self.canonical_home.components().any(|part|matches!(part,std::path::Component::ParentDir|std::path::Component::CurDir)) {
            return Err("Invalid native history physical account origin".into());
        }
        Ok(())
    }
}

/// These are different provider proofs, not interchangeable native turn IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TurnIdentityKind {
    /// A native thread/turn pair, such as Codex's threadId and turn.id.
    NativeTurn,
    /// A confirmed accepted user input, such as Claude's received user.uuid
    /// joined to command_lifecycle.command_uuid. Never an assistant/result UUID.
    AcceptedInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnIdentity {
    pub session_id: String,
    pub kind: TurnIdentityKind,
    pub id: String,
}

impl TurnIdentity {
    pub fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.session_id) || !valid_id(&self.id) {
            return Err("Invalid confirmed turn identity".into());
        }
        Ok(())
    }
}

/// Immutable producer evidence. Bindings can arrive after a terminal result;
/// neither update is permission to settle another turn's current activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnUpdate {
    Bind { identity: TurnIdentity, item_ids: Vec<String> },
    Finish { identity: TurnIdentity, outcome: ActivityOutcome, finished_at: u64 },
}

impl TurnUpdate {
    pub fn identity(&self) -> &TurnIdentity {
        match self { Self::Bind { identity, .. } | Self::Finish { identity, .. } => identity }
    }

    pub fn validate(&self) -> Result<(), String> {
        self.identity().validate()?;
        match self {
            Self::Bind { item_ids, .. } => {
                if item_ids.is_empty() || item_ids.len() > MAX_TURN_ITEM_IDS {
                    return Err("Turn item binding exceeds its bounded identity window".into());
                }
                let mut seen = HashSet::new();
                if item_ids.iter().any(|id| !valid_id(id) || !seen.insert(id.as_str())) {
                    return Err("Invalid or duplicate turn item identity".into());
                }
                let bytes = 64 + self.identity().session_id.len() + self.identity().id.len()
                    + item_ids.iter().map(|id| id.len() + 8).sum::<usize>();
                if bytes > MAX_TURN_UPDATE_BYTES { return Err("Turn binding exceeds its byte budget; split the binding batch".into()); }
            }
            Self::Finish { outcome, .. } => validate_outcome(outcome)?,
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnOutcome {
    pub identity: TurnIdentity,
    pub outcome: ActivityOutcome,
    /// First observed terminal time, retained unchanged through replay/restart.
    pub finished_at: u64,
}

impl TurnOutcome {
    pub fn validate(&self) -> Result<(), String> {
        self.identity.validate()?;
        validate_outcome(&self.outcome)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleTurnOutcome {
    pub outcome: TurnOutcome,
    /// Exact IDs in Snapshot.items. Store supplies native-part ownership;
    /// consumers must not parse IDs or infer provider identity from layout.
    pub item_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnOutcomeProjection {
    pub revision: u64,
    pub records: Vec<VisibleTurnOutcome>,
    /// False includes old snapshots/unqualified identity and a bounded partial
    /// projection. Missing records never imply that a turn succeeded.
    pub complete: bool,
}

/// Constructed while paging the transcript, where the original item owning a
/// native-part ID is still known. This is never inferred by a UI reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisibleItemOrigin {
    pub item_id: String,
    pub source_item_id: String,
}

/// Local storage acknowledgement, deliberately absent from the producer wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TurnWriteReceipt {
    pub revision: u64,
    pub changed: bool,
}

impl TurnOutcomeProjection {
    pub fn validate(&self) -> Result<(), String> {
        if self.records.len() > MAX_PROJECTED_TURNS {
            return Err("Turn outcome projection exceeds its record budget".into());
        }
        let mut turns = HashSet::new();
        let mut items = HashSet::new();
        let mut bytes = 32usize;
        for record in &self.records {
            record.outcome.validate()?;
            bytes = bytes.saturating_add(64 + record.outcome.identity.session_id.len()
                + record.outcome.identity.id.len() + record.outcome.outcome.reason().map_or(0, str::len));
            if !turns.insert(&record.outcome.identity) || record.item_ids.is_empty()
                || record.item_ids.len() > MAX_TURN_ITEM_IDS {
                return Err("Invalid or duplicate projected turn".into());
            }
            if record.item_ids.iter().any(|id| id.is_empty() || id.len() > MAX_TURN_ID_BYTES + 256 || !items.insert(id.as_str())) {
                return Err("A projected item has invalid or conflicting ownership".into());
            }
            for id in &record.item_ids { bytes = bytes.saturating_add(8 + id.len()); }
            if bytes > MAX_PROJECTION_BYTES { return Err("Turn outcome projection exceeds its byte budget".into()); }
        }
        if postcard::to_stdvec(self).map_err(|error| error.to_string())?.len() > MAX_PROJECTION_BYTES {
            return Err("Turn outcome projection exceeds its byte budget".into());
        }
        Ok(())
    }
}

/// Bounded parent history-source proof. This carries no turn outcome,
/// original-command eligibility, live-session authority or child attribution.
pub fn validate_history_origins(session:&str,item_ids:&[String]) -> Result<(),String> {
    if !valid_id(session) || item_ids.is_empty() || item_ids.len()>MAX_TURN_ITEM_IDS {
        return Err("Invalid history origin window".into());
    }
    let mut seen=HashSet::new();
    if item_ids.iter().any(|id|!valid_id(id)||!seen.insert(id.as_str())) {
        return Err("Invalid or duplicate history source alias".into());
    }
    if 64+session.len()+item_ids.iter().map(|id|id.len()+8).sum::<usize>()>MAX_TURN_UPDATE_BYTES {
        return Err("History origin proof exceeds its byte budget".into());
    }
    Ok(())
}

fn valid_id(id: &str) -> bool { !id.is_empty() && id.len() <= MAX_TURN_ID_BYTES }

fn validate_outcome(outcome: &ActivityOutcome) -> Result<(), String> {
    if outcome.reason().is_some_and(|reason| reason.len() > MAX_TURN_REASON_BYTES) {
        return Err("Turn outcome reason exceeds its byte budget".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(id: &str) -> TurnIdentity {
        TurnIdentity { session_id: "confirmed-session".into(), kind: TurnIdentityKind::AcceptedInput, id: id.into() }
    }
    fn record(id: &str, item: &str, outcome: ActivityOutcome) -> VisibleTurnOutcome {
        VisibleTurnOutcome { outcome: TurnOutcome { identity: identity(id), outcome, finished_at: 1234 }, item_ids: vec![item.into()] }
    }

    #[test]
    fn accepted_input_and_native_turn_proofs_are_distinct_and_round_trip() {
        let accepted = identity("provider-id");
        let mut native = accepted.clone(); native.kind = TurnIdentityKind::NativeTurn;
        assert_ne!(accepted, native);
        for update in [TurnUpdate::Bind { identity: accepted.clone(), item_ids: vec!["actual-user:input/0".into()] },
            TurnUpdate::Finish { identity: accepted, outcome: ActivityOutcome::Cancelled, finished_at: 1234 }] {
            update.validate().unwrap();
            assert_eq!(postcard::from_bytes::<TurnUpdate>(&postcard::to_stdvec(&update).unwrap()).unwrap(), update);
        }
    }

    #[test]
    fn earlier_cancelled_turn_coexists_with_later_failure_without_rekeying() {
        let projection = TurnOutcomeProjection { revision: 2, complete: true, records: vec![
            record("A", "input-A", ActivityOutcome::Cancelled),
            record("B", "input-B", ActivityOutcome::Failed { reason: "provider exited with code 3".into() }),
        ] };
        projection.validate().unwrap();
        assert_eq!(postcard::from_bytes::<TurnOutcomeProjection>(&postcard::to_stdvec(&projection).unwrap()).unwrap(), projection);
        assert_eq!(projection.records[0].outcome.identity.id, "A");
        assert_eq!(projection.records[0].outcome.outcome, ActivityOutcome::Cancelled);
    }

    #[test]
    fn unqualified_legacy_projection_is_not_success_and_conflicting_joins_refuse() {
        assert!(!TurnOutcomeProjection::default().complete);
        let mut projection = TurnOutcomeProjection { revision: 1, complete: true, records: vec![
            record("A", "same-item", ActivityOutcome::Succeeded), record("B", "same-item", ActivityOutcome::Cancelled),
        ] };
        assert!(projection.validate().is_err());
        projection.records.pop(); projection.validate().unwrap();
        projection.records[0].outcome.outcome = ActivityOutcome::Failed { reason: "x".repeat(MAX_TURN_REASON_BYTES + 1) };
        assert!(projection.validate().is_err());
    }

    #[test]
    fn terminal_projection_does_not_replace_current_turn_scalars() {
        let mut snapshot = crate::chat::Snapshot { turn_generation: 2, busy: true, ..Default::default() };
        snapshot.turn_outcomes = std::sync::Arc::new(TurnOutcomeProjection { revision: 3, complete: true,
            records: vec![record("A", "input-A", ActivityOutcome::Cancelled)] });
        let decoded: crate::chat::Snapshot = postcard::from_bytes(&postcard::to_stdvec(&snapshot).unwrap()).unwrap();
        assert!(decoded.busy); assert_eq!(decoded.turn_generation, 2); assert!(decoded.outcome.is_none());
        assert_eq!(decoded.turn_outcomes.records[0].outcome.identity.id, "A");
    }
}
