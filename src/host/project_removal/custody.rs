//! Exact project-operation continuation. The existing scoped reservation owns
//! one prepared phase, including original FDs and retained owners. Final busy
//! is a pending result, never permission to release or repeat those effects.
use super::*;
use super::super::model::project_operations::{self,Phase,Obligation,SavedScope};
use crate::project_recovery::{ProjectOperationRequest as Request,ProjectOperationAction as Action,
    ProjectOperationOutcome as Outcome,ProjectOperationPhase as PublicPhase};
use crate::workspace_model::WorkspaceModel;
use std::sync::{Arc,Mutex,atomic::{AtomicBool,Ordering}};

// Prevent simultaneous retained repair plans from exhausting daemon FDs/RAM.
// Durable restart-negative rows have their separate64-entry budget.
const MAX_LIVE_OPERATIONS:usize=4;
const MAX_RETAINED_DIRECTORIES:usize=1024;
const MAX_RETAINED_BYTES:usize=16*1024*1024;

enum Work {
    Preparing,
    Locate(super::super::session_locations::PreparedRelocation),
    Remove(PreparedRemoval),
    Lost,
    Complete,
}
pub(super) struct Operation {
    pub(super) operation:uuid::Uuid,
    pub(super) project:ProjectInstance,
    pub(super) request:Option<Request>,
    request_digest:String,
    commit_digest:String,
    work:Mutex<Work>,
    claimed:AtomicBool,
    settled:AtomicBool,
}
impl Operation {
    pub(super) fn is_settled(&self)->bool {self.settled.load(Ordering::Acquire)}
    #[cfg(test)]
    pub(super) fn retains_prepared(&self)->bool {
        matches!(*self.work.lock().unwrap_or_else(|p|p.into_inner()),Work::Locate(_)|Work::Remove(_))
    }
    pub(super) fn lost(row:&Obligation,scope:&SavedScope)->Result<Self,String> {
        Ok(Self{operation:row.operation,project:scope.project.clone(),request:None,
            request_digest:row.request_digest.clone(),commit_digest:row.commit_digest.clone(),
            work:Mutex::new(Work::Lost),claimed:AtomicBool::new(false),settled:AtomicBool::new(false)})
    }
}
struct Claim(Arc<Operation>);
impl Drop for Claim {fn drop(&mut self){self.0.claimed.store(false,Ordering::Release);}}

/// Private capability built only after exact request/lease/phase admission.
/// SQL receipt comparison and original edit validation remain inside its TX.
pub(in crate::host) struct CommitAuthority<'a> {
    operation:&'a Operation,
    reservation:&'a Reservation,
}
impl CommitAuthority<'_> {
    #[cfg(test)]
    pub(in crate::host) fn before_final_commit(&self,host:&SharedHost) {
        let hook=host.lock().unwrap_or_else(|p|p.into_inner()).project_operation_before_commit.clone();
        if let Some(hook)=hook {hook(self.operation());}
    }
    pub(in crate::host) fn operation(&self)->uuid::Uuid {self.operation.operation}
    pub(in crate::host) fn request_digest(&self)->&str {&self.operation.request_digest}
    pub(in crate::host) fn ensure_reservation(&self,edit:&ModelEdit)->Result<(),ProtocolError> {
        self.reservation.ensure_operation_commit(edit,self.operation)
    }
    fn phase(&self,host:&SharedHost)->Result<Phase,ProtocolError> {
        let registry=super::super::agent_execution::registry(host).map_err(error)?;
        let row=registry.lock().unwrap_or_else(|p|p.into_inner()).project_operation(self.operation()).map_err(error)?
            .ok_or_else(||error("The original project operation custody journal is missing"))?;
        if row.request_digest!=self.request_digest() || row.commit_digest!=self.operation.commit_digest {
            return Err(error("The original project operation journal changed"));
        }
        Ok(row.phase)
    }
    fn advance_phase(&self,host:&SharedHost,from:Phase,to:Phase)->Result<(),ProtocolError> {
        let registry=super::super::agent_execution::registry(host).map_err(error)?;
        let result=registry.lock().unwrap_or_else(|p|p.into_inner()).advance_project_operation(self.operation(),self.request_digest(),from,to).map_err(error);
        result
    }
    pub(in crate::host) fn model_is_committed(&self,host:&SharedHost)->Result<bool,ProtocolError> {
        Ok(matches!(self.phase(host)?,Phase::ModelCommitted))
    }
    pub(in crate::host) fn before_repair(&self,host:&SharedHost)->Result<(),ProtocolError> {
        match self.phase(host)? {Phase::Admitted=>self.advance_phase(host,Phase::Admitted,Phase::Repairing),Phase::Repairing|Phase::PendingCommit=>Ok(()),_=>Err(error("Locate no longer owns its original repair phase"))}
    }
    pub(in crate::host) fn before_close(&self,host:&SharedHost)->Result<(),ProtocolError> {
        match self.phase(host)? {Phase::Admitted=>self.advance_phase(host,Phase::Admitted,Phase::Closing),Phase::Closing|Phase::PendingCommit=>Ok(()),_=>Err(error("Removal no longer owns its original owner-close phase"))}
    }
    pub(in crate::host) fn pending_commit(&self,host:&SharedHost)->Result<(),ProtocolError> {
        match self.phase(host)? {
            Phase::Admitted=>self.advance_phase(host,Phase::Admitted,Phase::PendingCommit),
            Phase::Repairing=>self.advance_phase(host,Phase::Repairing,Phase::PendingCommit),
            Phase::Closing=>self.advance_phase(host,Phase::Closing,Phase::PendingCommit),
            Phase::PendingCommit=>Ok(()),_=>Err(error("The original project operation cannot enter pending commit")),
        }
    }
}

pub(in crate::host) fn restore(registry:&super::super::model::Registry,entries:&Reservations)->Result<(),String> {
    super::reservation::restore_operations(registry,entries)
}

fn pending(operation:uuid::Uuid,phase:PublicPhase,reason:impl Into<String>)->Result<(ServerResponse,Vec<ServerMessage>,bool),ProtocolError> {
    Ok((ServerResponse::ProjectOperation{outcome:Outcome::Pending{operation,phase,reason:reason.into()}},Vec::new(),false))
}
/// A COMMIT response can be lost after SQLite positively settled the row.
/// Re-reading that exact immutable receipt releases only its original lease;
/// it never repeats effects or lets an unrelated operation consume custody.
fn settle_original_receipt(host:&SharedHost,operation:uuid::Uuid) {
    if let Some((reservation,owner))=super::reservation::retained_operation(host,operation) {
        owner.settled.store(true,Ordering::Release);
        if let Ok(mut work)=owner.work.try_lock() {*work=Work::Complete;}
        drop(reservation);
    }
}
fn public_phase(phase:Phase)->PublicPhase {
    match phase {Phase::Admitted=>PublicPhase::Preparing,Phase::Repairing=>PublicPhase::Repairing,
        Phase::Closing=>PublicPhase::Closing,Phase::PendingCommit=>PublicPhase::PendingCommit,
        Phase::ModelCommitted=>PublicPhase::ModelCommitted,Phase::Resolved|Phase::Aborted=>PublicPhase::UnavailableAfterRestart}
}

pub(in crate::host) fn process(host:&SharedHost,client:ClientId,request:Request)->Result<(ServerResponse,Vec<ServerMessage>,bool),ProtocolError> {
    request.validate().map_err(error)?;
    let digest=project_operations::digest(&request).map_err(error)?;
    let commit_digest=project_operations::digest(&request.commit_edit()).map_err(error)?;
    let registry=super::super::agent_execution::registry(host).map_err(error)?;
    let gate=super::super::client::control_gate(host);
    let (reservation,operation,claim)={
        let _writer=super::super::client::try_scoped_control_write(&gate)?;
        super::super::client::ensure_control_open(host)?;
        super::super::agent_execution::terminals::admit(host,client,&ClientRequest::ProjectOperation{request:request.clone()})?;
        if let Some(reason)=&host.lock().unwrap_or_else(|p|p.into_inner()).project_operation_startup_error {return Err(error(reason.clone()));}
        let existing=registry.lock().unwrap_or_else(|p|p.into_inner()).project_operation(request.operation).map_err(error)?;
        if let Some(row)=existing {
            if row.request_digest!=digest || row.commit_digest!=commit_digest {return Err(error("This operation belongs to another original project request"));}
            match row.phase {
                Phase::Resolved=> {
                    settle_original_receipt(host,request.operation);
                    let model=registry.lock().unwrap_or_else(|p|p.into_inner()).model().map_err(error)?;
                    if model.identity!=request.project.registry {return Err(error("The original operation receipt belongs to a different registry"));}
                    return Ok((ServerResponse::ProjectOperation{outcome:Outcome::Committed{operation:request.operation,model}},Vec::new(),false));
                }
                Phase::Aborted=> {
                    settle_original_receipt(host,request.operation);
                    return Err(error("This operation ended before effects. Start a fresh explicit project action."));
                }
                _=>{},
            }
            let Some((reservation,operation))=super::reservation::retained_operation(host,request.operation) else {
                return pending(request.operation,PublicPhase::UnavailableAfterRestart,"This project operation lost its original prepared owners and directory pins. Its negative reservation remains; it cannot be replayed from pathnames.");
            };
            if operation.request.as_ref()!=Some(&request) {
                return pending(request.operation,PublicPhase::UnavailableAfterRestart,"The original prepared custody is unavailable after restart. The affected project remains reserved; no repair or close was repeated.");
            }
            if operation.claimed.compare_exchange(false,true,Ordering::AcqRel,Ordering::Acquire).is_err() {
                return pending(request.operation,public_phase(row.phase),"This exact project operation is already running. Its reservation and original sources were retained.");
            }
            let claim=Claim(operation.clone());
            if matches!(*operation.work.lock().unwrap_or_else(|p|p.into_inner()),Work::Preparing) && row.phase!=Phase::Admitted {
                return pending(request.operation,PublicPhase::UnavailableAfterRestart,"The durable phase already records effects but original prepared custody is absent. No repair or close was replayed.");
            }
            (reservation,operation,claim)
        } else if let Some((reservation,operation))=super::reservation::retained_operation(host,request.operation) {
            if operation.request.as_ref()!=Some(&request) || operation.request_digest!=digest
                || operation.claimed.compare_exchange(false,true,Ordering::AcqRel,Ordering::Acquire).is_err() {
                return pending(request.operation,PublicPhase::UnavailableAfterRestart,"The original project-operation admission is uncertain or already running; its reservation was preserved.");
            }
            let claim=Claim(operation.clone());
            if !matches!(*operation.work.lock().unwrap_or_else(|p|p.into_inner()),Work::Preparing) {
                return pending(request.operation,PublicPhase::UnavailableAfterRestart,"The durable custody row disappeared after preparation. Original effects were not repeated; the affected scope remains reserved.");
            }
            let scope=reservation.saved_scope(match &request.action {Action::Locate{relocation}=>Some(relocation.clone()),Action::Remove{..}=>None})?;
            registry.lock().unwrap_or_else(|p|p.into_inner()).begin_project_operation(request.operation,&digest,&commit_digest,&scope).map_err(error)?;
            (reservation,operation,claim)
        } else {
            if super::reservation::live_operation_count(host)>=MAX_LIVE_OPERATIONS {return Err(ProtocolError::new(ProtocolErrorCode::Internal,"The daemon is retaining its project-operation capacity. Retry after an existing operation settles.",true));}
            super::admit(host,&ClientRequest::EditWorkspaceModel{edit:request.commit_edit()})?;
            let reservation=match &request.action {
                Action::Locate{relocation}=>super::reserve_relocation(host,&request.project,relocation)?,
                Action::Remove{sessions}=>super::reserve(host,&request.project,sessions)?,
            };
            let scope=reservation.saved_scope(match &request.action {Action::Locate{relocation}=>Some(relocation.clone()),Action::Remove{..}=>None})?;
            let operation=Arc::new(Operation{operation:request.operation,project:request.project.clone(),request:Some(request.clone()),request_digest:digest.clone(),commit_digest:commit_digest.clone(),
                work:Mutex::new(Work::Preparing),claimed:AtomicBool::new(true),settled:AtomicBool::new(false)});
            reservation.bind_operation(operation.clone())?;
            let admission_result=registry.lock().unwrap_or_else(|p|p.into_inner()).begin_project_operation(request.operation,&digest,&commit_digest,&scope);
            if let Err(reason)=admission_result {
                // Release only after positive absence. Ambiguous storage failure
                // retains the negative reservation and refuses unsafe retry.
                if registry.lock().unwrap_or_else(|p|p.into_inner()).project_operation(request.operation).is_ok_and(|row|row.is_none()) {operation.settled.store(true,Ordering::Release);}
                // No preparation/effect has run. If storage was ambiguous, keep
                // Preparing so exact retry can positively recover this admission.
                operation.claimed.store(false,Ordering::Release);
                return Err(error(format!("Cannot durably admit this project operation before effects: {reason}")));
            }
            let claim=Claim(operation.clone());(reservation,operation,claim)
        }
    };
    let authority=CommitAuthority{operation:&operation,reservation:&reservation};
    // This mutex is per operation. No HostState/global admission is held during
    // preparation, repair, provider waits or retained cleanup.
    let mut work=operation.work.lock().unwrap_or_else(|p|p.into_inner());
    if matches!(*work,Work::Preparing) {
        let _admitted=gate.read().unwrap_or_else(|p|p.into_inner());
        super::super::client::ensure_control_open(host)?;
        let prepared=match &request.action {
            Action::Locate{relocation}=>super::super::session_locations::prepare_relocation(host,client,request.project.clone(),relocation.clone()).and_then(|prepared| {
                if prepared.retained_bytes()>MAX_RETAINED_BYTES || prepared.retained_directories()>MAX_RETAINED_DIRECTORIES {return Err(error("Locate exceeds its retained proof capacity; no metadata repair was started"));}
                Ok(Work::Locate(prepared))
            }),
            Action::Remove{sessions}=>super::prepare_removal(host,client,request.project.clone(),sessions.clone(),&reservation).map(Work::Remove),
        };
        match prepared {
            Ok(prepared)=>*work=prepared,
            Err(reason)=> {
                if authority.advance_phase(host,Phase::Admitted,Phase::Aborted).is_ok() {operation.settled.store(true,Ordering::Release);*work=Work::Complete;return Err(reason);}
                return pending(operation.operation,PublicPhase::Preparing,format!("{} The original operation remains reserved because its pre-effect abort could not be persisted.",reason.message));
            }
        }
    }
    let result=match &mut *work {
        Work::Locate(prepared)=>prepared.advance(host,client,&authority),
        Work::Remove(prepared)=>prepared.advance(host,client,&authority),
        Work::Lost=>return pending(operation.operation,PublicPhase::UnavailableAfterRestart,"Original prepared custody is unavailable. The affected scope remains reserved; no effects were repeated."),
        Work::Preparing|Work::Complete=>return Err(error("The original project operation has no resumable phase")),
    };
    match result {
        Ok(model)=> {
            if let Err(reason)=authority.advance_phase(host,Phase::ModelCommitted,Phase::Resolved) {
                return pending(operation.operation,PublicPhase::ModelCommitted,format!("The model commit completed, but its final custody receipt is pending: {}",reason.message));
            }
            #[cfg(test)] {
                // Owning fixture simulates a lost settlement completion AFTER
                // the real SQL COMMIT, before releasing original heavy custody.
                let hook=host.lock().unwrap_or_else(|p|p.into_inner()).project_operation_after_settlement.clone();
                if let Some(hook)=hook && let Err(reason)=hook(operation.operation) {
                    return pending(operation.operation,PublicPhase::ModelCommitted,reason);
                }
            }
            operation.settled.store(true,Ordering::Release);*work=Work::Complete;
            drop(work);drop(claim);drop(reservation);
            Ok((ServerResponse::ProjectOperation{outcome:Outcome::Committed{operation:operation.operation,model}},Vec::new(),false))
        }
        Err(reason)=> {
            let phase=authority.phase(host).map(public_phase).unwrap_or(PublicPhase::UnavailableAfterRestart);
            pending(operation.operation,phase,format!("{} This exact operation and any completed repair/owner-close progress remain retained. Retry this operation; do not start a replacement project action.",reason.message))
        }
    }
}
