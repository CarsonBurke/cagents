//! Window-scoped terminal annotations. No provider work or current activity
//! changes occur while reading an independently committed historical outcome.
use super::{Snapshot,annotations};
use crate::chat::turn_outcomes::{TurnOutcomeProjection,VisibleItemOrigin};
use std::sync::Arc;

#[derive(Debug,Clone,PartialEq,Eq)]
pub(super) struct ScopedOrigin {
    /// Captured when the actual source item entered the transcript cache.
    /// A later Session event never requalifies an older unknown/source item.
    pub session_id:Option<String>,
    pub origin:VisibleItemOrigin,
}

pub(super) fn refresh(
    authority: &mut annotations::Authority, origins:&[ScopedOrigin], state:&mut Snapshot,
) -> bool {
    let mut projection=authority.turn_outcomes(state.session_id.as_deref(),origins)
        .unwrap_or_else(|_|Arc::new(TurnOutcomeProjection::default()));
    if origins.len()!=state.items.len() && projection.complete { Arc::make_mut(&mut projection).complete=false; }
    if state.turn_outcomes==projection { return false; }
    state.turn_outcomes=projection;
    state.generation=state.generation.saturating_add(1);
    true
}

pub(super) fn push_origin(origins:&mut Vec<ScopedOrigin>,session:Option<&str>,item:&str,source:&str) {
    use crate::chat::turn_outcomes::MAX_TURN_ID_BYTES;
    if source.len()>MAX_TURN_ID_BYTES || item.len()>MAX_TURN_ID_BYTES+256 { return; }
    origins.push(ScopedOrigin { session_id:session.map(str::to_owned),
        origin:VisibleItemOrigin { item_id:item.to_owned(),source_item_id:source.to_owned() } });
}
