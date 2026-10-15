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
    pub account:Option<crate::chat::turn_outcomes::HistoryAccountOrigin>,
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

pub(super) fn push_origin(origins:&mut Vec<ScopedOrigin>,session:Option<&str>,account:Option<&crate::chat::turn_outcomes::HistoryAccountOrigin>,item:&str,source:&str) {
    use crate::chat::turn_outcomes::MAX_TURN_ID_BYTES;
    if source.len()>MAX_TURN_ID_BYTES || item.len()>MAX_TURN_ID_BYTES+256 { return; }
    origins.push(ScopedOrigin { session_id:session.map(str::to_owned),account:account.cloned(),
        origin:VisibleItemOrigin { item_id:item.to_owned(),source_item_id:source.to_owned() } });
}

/// Bounded sidecars are staged before preflight/step ingestion even when the
/// History event precedes its sidecar. Conflicting aliases remain unknown.
#[derive(Debug,Clone,PartialEq,Eq)]
pub(super) struct HistoryProof {
    pub session_id:String,
    pub account:crate::chat::turn_outcomes::HistoryAccountOrigin,
}
pub(super) fn history_origins(events:&[crate::chat::Event]) -> std::collections::HashMap<String,Option<HistoryProof>> {
    use crate::chat::{Event,turn_outcomes::validate_history_origins};
    let mut origins:std::collections::HashMap<String,Option<HistoryProof>>=Default::default();let mut bytes=0usize;
    for event in events {
        let Event::HistoryOrigins{account,session_id,item_ids}=event else {continue;};
        if account.validate().is_err() || validate_history_origins(account,session_id,item_ids).is_err() {continue;}
        if physical_scope(account).is_err() {continue;}
        let proof=HistoryProof{session_id:session_id.clone(),account:account.clone()};
        for id in item_ids {
            if let Some(previous)=origins.get_mut(id) {
                if previous.as_ref()!=Some(&proof) { *previous=None; }
                continue;
            }
            let additional=id.len()+session_id.len()+account.canonical_home.as_os_str().len()+128;
            if bytes.saturating_add(additional)>256*1024 {continue;}
            bytes+=additional;origins.insert(id.clone(),Some(proof.clone()));
        }
    }
    origins
}

pub(super) fn account_origin(scope:&crate::account::scope::LaunchAccountScope) -> crate::chat::turn_outcomes::HistoryAccountOrigin {
    crate::chat::turn_outcomes::HistoryAccountOrigin {provider:scope.provider,canonical_home:scope.canonical_home.clone(),device:scope.device,inode:scope.inode}
}
pub(super) fn physical_scope(origin:&crate::chat::turn_outcomes::HistoryAccountOrigin) -> Result<crate::account::scope::LaunchAccountScope,String> {
    origin.validate()?;
    let scope=crate::account::scope::LaunchAccountScope {provider:origin.provider,canonical_home:origin.canonical_home.clone(),device:origin.device,inode:origin.inode,registered_id:None,purge_ids:vec![]};
    scope.revalidate()?;Ok(scope)
}
