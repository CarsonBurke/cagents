//! Accepted-input observation evidence is separate from native terminal fate.
//! These values carry no authority to stop tools or settle current activity.
use super::{MAX_TURN_ID_BYTES, MAX_TURN_ITEM_IDS, MAX_TURN_UPDATE_BYTES, MAX_PROJECTION_BYTES, TurnIdentity, TurnIdentityKind};
use crate::agent::Provider;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use uuid::Uuid;

pub const MAX_OBSERVATION_REASON_BYTES: usize = 4096;

/// The identity is minted only after this actual backend has opened. Numeric
/// process fields are metadata; a producer needs its owned capability for exit
/// status. A launcher or protocol leader is not proof of descendant death.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservationBackend {
    OwnedProtocolProcess { instance: Uuid, pid: u32, start_ticks: Option<u64> },
    RetainedProvider { instance: Uuid, pid: u32, start_ticks: u64 },
    ConnectedSocket { instance: Uuid, device: u64, inode: u64 },
}
impl ObservationBackend {
    pub fn instance(&self) -> Uuid {
        match self { Self::OwnedProtocolProcess {instance,..} | Self::RetainedProvider {instance,..}
            | Self::ConnectedSocket {instance,..} => *instance }
    }
    fn validate(&self) -> Result<(), String> {
        if self.instance().is_nil() {return Err("Missing actual observation backend instance".into());}
        match self {
            Self::OwnedProtocolProcess {pid,start_ticks,..} if !valid_pid(*pid) || *start_ticks == Some(0) => Err("Invalid owned protocol process witness".into()),
            Self::RetainedProvider {pid,start_ticks,..} if !valid_pid(*pid) || *start_ticks == 0 => Err("Invalid retained provider witness".into()),
            Self::ConnectedSocket {inode,..} if *inode == 0 => Err("Invalid connected socket witness".into()),
            _ => Ok(()),
        }
    }
}
fn valid_pid(pid: u32) -> bool {pid > 0 && pid <= i32::MAX as u32}

/// One actual backend's observation of one confirmed native session. A new
/// accepted native session creates a different scope even on the same backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputObserver {
    pub id: Uuid,
    pub actor: Uuid,
    pub provider: Provider,
    pub native_session: String,
    pub backend: ObservationBackend,
    /// Mismatch fence for the original physical annotation namespace. This
    /// digest is not authority to reopen an account or replace its capture.
    pub account_key: [u8;32],
}
impl InputObserver {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_nil() || self.actor.is_nil() || self.account_key == [0;32] || !super::valid_id(&self.native_session)
            || !matches!(self.provider,Provider::Claude | Provider::Codex) {
            return Err("Invalid captured native-session observation scope".into());
        }
        self.backend.validate()
    }
}

/// This exact input was accepted. Optional native-turn ownership is frozen at
/// capture; neither aliases nor a later current turn can fill in a missing join.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedInputObservation {
    pub input: TurnIdentity,
    pub native_turn: Option<TurnIdentity>,
    pub observer: InputObserver,
    pub source_incarnation: Uuid,
    /// First local observation time, not an invented native acceptance time.
    pub observed_at: u64,
}
impl AcceptedInputObservation {
    pub fn validate(&self) -> Result<(), String> {
        self.input.validate()?; self.observer.validate()?;
        if self.input.kind != TurnIdentityKind::AcceptedInput || self.input.session_id != self.observer.native_session
            || self.source_incarnation.is_nil() || self.observed_at > i64::MAX as u64 {
            return Err("Invalid exact accepted-input observation".into());
        }
        if let Some(turn) = &self.native_turn {
            turn.validate()?;
            if turn.kind != TurnIdentityKind::NativeTurn || turn.session_id != self.input.session_id {
                return Err("Native terminal owner does not match the captured input session".into());
            }
        }
        Ok(())
    }
    /// Both are exact captured identities. A known native-turn relationship
    /// never erases a directly correlated accepted-input terminal proof.
    pub fn accepts_terminal_identity(&self, identity: &TurnIdentity) -> bool {
        &self.input == identity || self.native_turn.as_ref() == Some(identity)
    }
}

/// Only user aliases proved for this exact accepted input and original scope.
/// This append is not a new accepted observation or native-turn association.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedInputAliases {
    pub input: TurnIdentity,
    pub observer_id: Uuid,
    pub item_ids: Vec<String>,
}
impl AcceptedInputAliases {
    pub fn validate(&self) -> Result<(), String> {
        validate_user_aliases(&self.input,self.observer_id,&self.item_ids,MAX_TURN_ID_BYTES,MAX_TURN_UPDATE_BYTES)
    }
}
fn validate_user_aliases(input: &TurnIdentity, observer_id: Uuid, ids: &[String], id_limit: usize, byte_limit: usize) -> Result<(), String> {
    input.validate()?;
    if input.kind != TurnIdentityKind::AcceptedInput || observer_id.is_nil()
        || ids.is_empty() || ids.len() > MAX_TURN_ITEM_IDS {
        return Err("Invalid accepted-input user alias append".into());
    }
    let mut seen=HashSet::new();
    let mut bytes=96+input.session_id.len()+input.id.len();
    for id in ids {
        if id.is_empty() || id.len()>id_limit || !seen.insert(id) {
            return Err("Invalid or duplicate accepted-input user alias".into());
        }
        bytes=bytes.saturating_add(id.len()+8);
        if bytes>byte_limit {return Err("Accepted-input aliases exceed their byte budget".into());}
    }
    Ok(())
}

/// Status is scoped only to the witnessed process. Even its positive kernel
/// exit is not a native input result or proof that remote/MCP work ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservationProcessRole { OwnedProtocolLeader, ProvedRetainedProvider }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservationLossCause {
    KernelProcessExit { role: ObservationProcessRole, code: Option<i32>, signal: Option<i32> },
    ObserverDisconnected,
    ObserverFailed { reason: String },
    ObserverClosed,
    ObservationSessionChanged,
}
impl ObservationLossCause {
    fn validate(&self, backend: &ObservationBackend) -> Result<(), String> {
        match self {
            Self::KernelProcessExit {role,code,signal} => {
                if !matches!((role,backend),
                    (ObservationProcessRole::OwnedProtocolLeader,ObservationBackend::OwnedProtocolProcess {..})
                    | (ObservationProcessRole::ProvedRetainedProvider,ObservationBackend::RetainedProvider {..}))
                    || !matches!((code,signal),(Some(0..=255),None)|(None,Some(1..=127))) {
                    return Err("Kernel exit lacks a matching process role/status witness".into());
                }
            }
            Self::ObserverFailed {reason} if reason.is_empty() || reason.len()>MAX_OBSERVATION_REASON_BYTES => {
                return Err("Observation failure reason exceeds its bounded diagnostic".into());
            }
            _=>{},
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputObservationLost {
    pub observer: InputObserver,
    pub event_id: Uuid,
    pub observed_at: u64,
    pub cause: ObservationLossCause,
}
impl InputObservationLost {
    pub fn validate(&self) -> Result<(), String> {
        self.observer.validate()?;
        if self.event_id.is_nil() || self.observed_at>i64::MAX as u64 {return Err("Invalid observation-loss event identity/time".into());}
        self.cause.validate(&self.observer.backend)
    }
}

/// Append-only semantic evidence. Arrival order does not establish fate: a
/// matching native Finish permanently supersedes observational uncertainty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnObservationUpdate {
    Accepted(AcceptedInputObservation),
    AcceptedAliases(AcceptedInputAliases),
    Lost(InputObservationLost),
}
impl TurnObservationUpdate {
    pub fn session_id(&self) -> &str {
        match self {Self::Accepted(value)=>&value.input.session_id,
            Self::AcceptedAliases(value)=>&value.input.session_id,Self::Lost(value)=>&value.observer.native_session}
    }
    pub fn validate(&self) -> Result<(), String> {
        match self {Self::Accepted(value)=>value.validate()?,Self::AcceptedAliases(value)=>value.validate()?,Self::Lost(value)=>value.validate()?}
        if postcard::to_stdvec(self).map_err(|error|error.to_string())?.len()>MAX_TURN_UPDATE_BYTES {
            return Err("Observation evidence exceeds its semantic frame budget".into());
        }
        Ok(())
    }
}

/// Exact visible user aliases are supplied by the guarded durable projection.
/// Neither the input's execution state nor the fate of its tools is implied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleInputObservationLost {
    pub accepted: AcceptedInputObservation,
    pub loss: InputObservationLost,
    pub item_ids: Vec<String>,
}
impl VisibleInputObservationLost {
    pub fn validate(&self) -> Result<(), String> {
        self.accepted.validate()?; self.loss.validate()?;
        if self.accepted.observer != self.loss.observer {
            return Err("Observation loss does not belong to this exact accepted observer".into());
        }
        // Native source aliases and Store-generated display parts have
        // different bounds. The latter carry a bounded native-part suffix.
        let metadata=postcard::to_stdvec(&(&self.accepted,&self.loss)).map_err(|error|error.to_string())?.len();
        validate_user_aliases(&self.accepted.input,self.accepted.observer.id,&self.item_ids,
            MAX_TURN_ID_BYTES+256,MAX_PROJECTION_BYTES.saturating_sub(metadata))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observer() -> InputObserver {InputObserver {id:Uuid::new_v4(),actor:Uuid::new_v4(),provider:Provider::Claude,
        native_session:"native-session".into(),backend:ObservationBackend::OwnedProtocolProcess {instance:Uuid::new_v4(),pid:100,start_ticks:Some(1)},account_key:[1;32]}}
    fn accepted() -> AcceptedInputObservation {AcceptedInputObservation {input:TurnIdentity {session_id:"native-session".into(),kind:TurnIdentityKind::AcceptedInput,id:"accepted-user-uuid".into()},native_turn:None,
        observer:observer(),source_incarnation:Uuid::new_v4(),observed_at:1}}
    #[test]
    fn observation_evidence_round_trips_without_becoming_a_terminal_outcome() {
        let accepted=accepted();
        for update in [TurnObservationUpdate::Accepted(accepted.clone()),TurnObservationUpdate::AcceptedAliases(AcceptedInputAliases {input:accepted.input.clone(),observer_id:accepted.observer.id,item_ids:vec!["native-user-row".into()]}),
            TurnObservationUpdate::Lost(InputObservationLost {observer:accepted.observer,event_id:Uuid::new_v4(),observed_at:2,cause:ObservationLossCause::ObserverDisconnected})] {
            update.validate().unwrap();assert_eq!(postcard::from_bytes::<TurnObservationUpdate>(&postcard::to_stdvec(&update).unwrap()).unwrap(),update);
        }
    }
    #[test]
    fn foreign_session_terminal_owner_and_unbounded_aliases_refuse() {
        let mut accepted=accepted();accepted.native_turn=Some(TurnIdentity {session_id:"another-session".into(),kind:TurnIdentityKind::NativeTurn,id:"turn".into()});assert!(accepted.validate().is_err());
        accepted.native_turn=None;accepted.validate().unwrap();
        let aliases=AcceptedInputAliases {input:accepted.input,observer_id:accepted.observer.id,item_ids:(0..128).map(|i|format!("{i}:{}","x".repeat(4000))).collect()};assert!(aliases.validate().is_err());
    }
    #[test]
    fn native_terminal_resolution_matches_only_captured_exact_identities() {
        let mut accepted=accepted();
        let native=TurnIdentity {session_id:accepted.input.session_id.clone(),kind:TurnIdentityKind::NativeTurn,id:"exact-native-turn".into()};
        accepted.native_turn=Some(native.clone());
        assert!(accepted.accepts_terminal_identity(&accepted.input));
        assert!(accepted.accepts_terminal_identity(&native));
        let mut foreign=native.clone();foreign.session_id="other-session".into();
        assert!(!accepted.accepts_terminal_identity(&foreign));
        foreign=native;foreign.id="B".into();assert!(!accepted.accepts_terminal_identity(&foreign));
    }
    #[test]
    fn maximum_native_user_alias_can_publish_a_bounded_native_part_id() {
        let accepted=accepted();
        let native="n".repeat(MAX_TURN_ID_BYTES);
        AcceptedInputAliases {input:accepted.input.clone(),observer_id:accepted.observer.id,item_ids:vec![native.clone()]}.validate().unwrap();
        let visible=format!("{native}:native-part:0:131072");
        assert!(AcceptedInputAliases {input:accepted.input.clone(),observer_id:accepted.observer.id,item_ids:vec![visible.clone()]}.validate().is_err());
        let loss=InputObservationLost {observer:accepted.observer.clone(),event_id:Uuid::new_v4(),observed_at:2,cause:ObservationLossCause::ObserverDisconnected};
        VisibleInputObservationLost {accepted,loss,item_ids:vec![visible]}.validate().unwrap();
    }
    #[test]
    fn socket_eof_cannot_validate_as_kernel_provider_exit() {
        let mut observer=observer();observer.backend=ObservationBackend::ConnectedSocket {instance:Uuid::new_v4(),device:1,inode:2};
        let mut lost=InputObservationLost {observer,event_id:Uuid::new_v4(),observed_at:2,cause:ObservationLossCause::KernelProcessExit {role:ObservationProcessRole::ProvedRetainedProvider,code:Some(0),signal:None}};
        assert!(lost.validate().is_err());lost.cause=ObservationLossCause::ObserverDisconnected;lost.validate().unwrap();
    }
}
