//! Exact retained Claude actor. Only bounded semantic publications cross this
//! socket; provider stdout and mutable reducer state never leave the helper.
mod journal;
pub(crate) use journal::{InputIdentity,InputReceipt,InputState,OutboxEntry};
use journal::Journal;
use super::{LocalHandle,Rejection,Update};
use crate::{chat::{self,Command,Snapshot,protocol::{ProcessSpec,TransportSpec,ExecutionContext}},daemon::holder_registry::{self as registry,Kind,Process,Record},model::HostSessionProof};
use serde::{Serialize,Deserialize,de::DeserializeOwned};
use serde_json::{Value,json};
use std::{collections::BTreeMap,fs::File,io::{Read,Write},os::{fd::{AsRawFd,FromRawFd,RawFd},unix::{net::{UnixStream,UnixListener},process::CommandExt,fs::MetadataExt}},path::PathBuf,process::{Command as ChildCommand,Stdio},sync::{Arc,Mutex,atomic::{AtomicBool,Ordering}},thread,time::{Duration,Instant}};
const ARGUMENT:&str="--cagents-native-actor";
const PROTOCOL:u32=1;
const CAPABILITY:&str="claude-actor-v1";
const TIMEOUT:Duration=Duration::from_secs(5);
const MAX_FRAME:usize=34*1024*1024;
#[derive(Serialize,Deserialize)]
enum InputPayload {
    Command {run:Option<crate::model::RunId>,command:Command},
    Interrupt {run:Option<crate::model::RunId>,including_subagents:bool},
}


#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
pub(crate) struct ActorIdentity { pub instance:uuid::Uuid,pub record:Record }
#[derive(Clone,Serialize,Deserialize)]
pub(crate) struct Resident {
    pub generation:u64,
    pub session_id:Option<String>,
    pub title:Option<String>,
    pub ready:bool,
    pub busy:bool,
    pub waiting:bool,
    pub working:bool,
    pub activity:crate::agent::ActivityState,
    pub outcome:Option<crate::agent::ActivityOutcome>,
    pub usage:chat::Usage,
    pub native_continuity:chat::NativeContinuity,
    pub requests:Vec<chat::Request>,
    pub completed:bool,
    pub owns_work:bool,
    pub idle:bool,
    pub failure:Option<String>,
    pub confirmed_session:Option<String>,
    pub background_subagents:Vec<String>,
    pub interrupt_subagents:Vec<String>,
    pub account_reports:Vec<Update>,
    next_input:u64,
    outbox_ack:u64,
    changed:u64,
}
#[derive(Clone,Serialize,Deserialize)]
pub(crate) struct ActorInspection {
    pub identity:ActorIdentity,
    pub source:chat::Session,
    pub previous_sources:Vec<chat::Session>,
    pub state:Resident,
    pub execution_environment:crate::terminal_ipc::ChatExecutionEnvironment,
    pub recovery_environment:BTreeMap<String,String>,
}
#[derive(Serialize,Deserialize)]
struct Startup {
    protocol:u32,
    source:chat::Session,
    program:String,
    arguments:Vec<String>,
    environment:BTreeMap<String,String>,
    working_directory:PathBuf,
    directory_fd:Option<RawFd>,
    directory_identity:Option<crate::agent_execution::DirectoryIdentity>,
    recovery_environment:BTreeMap<String,String>,
}
#[derive(Serialize,Deserialize)]
struct Invocation { proof:HostSessionProof,epoch:Option<uuid::Uuid>,method:String,args:Value }
#[derive(Serialize,Deserialize)]
struct Attached { protocol:u32,identity:ActorIdentity,epoch:uuid::Uuid,resident:Resident,source:chat::Session,execution_environment:crate::terminal_ipc::ChatExecutionEnvironment,recovery_environment:BTreeMap<String,String> }
#[derive(Clone)]
pub(super) struct RemoteActor(Arc<Remote>);
struct Pending { identity:InputIdentity,payload:Value,bytes:usize,reply:Option<crossbeam_channel::Sender<Result<(),String>>> }
struct Remote {
    identity:ActorIdentity,
    epoch:uuid::Uuid,
    proof:HostSessionProof,
    socket:PathBuf,
    state:Mutex<Arc<Resident>>,
    environment:crate::terminal_ipc::ChatExecutionEnvironment,
    recovery_environment:BTreeMap<String,String>,
    input_sequence:Mutex<u64>,
    uncertain_input:Mutex<Option<InputIdentity>>,
    pending:Mutex<Vec<Pending>>,
    notify:Arc<dyn Fn(Update)+Send+Sync>,
    stop:AtomicBool,
    installed:AtomicBool,
    preserved:AtomicBool,
    worker:Mutex<Option<thread::JoinHandle<()>>>,
    startup:Mutex<Option<UnixStream>>,
}
struct StartupOwner {
    control:Mutex<Option<UnixStream>>,
    cancelled:AtomicBool,
    outcome:Mutex<chat::protocol::LaunchOutcome>,
    finished:std::sync::Condvar,
}
struct StartupAttempt {owner:Arc<StartupOwner>,handed_off:bool}
impl Drop for StartupAttempt {
    fn drop(&mut self) {
        if !self.handed_off {
            self.owner.cancelled.store(true,Ordering::Release);
            if let Some(control)=self.owner.control.lock().unwrap_or_else(|p|p.into_inner()).take() {let _=control.shutdown(std::net::Shutdown::Both);}
        }
    }
}
impl chat::protocol::LaunchCleanup for StartupOwner {
    fn cleanup(&self)->Result<chat::protocol::LaunchOutcome,String> {
        self.cancelled.store(true,Ordering::Release);
        if let Some(control)=self.control.lock().unwrap_or_else(|p|p.into_inner()).take() {let _=control.shutdown(std::net::Shutdown::Both);}
        let deadline=Instant::now()+Duration::from_secs(40);
        let mut outcome=self.outcome.lock().unwrap_or_else(|p|p.into_inner());
        while *outcome==chat::protocol::LaunchOutcome::Uncertain {
            let remaining=deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {return Err("Retained actor startup cleanup remains unconfirmed; its exact owner is retained".into());}
            outcome=self.finished.wait_timeout(outcome,remaining).unwrap_or_else(|p|p.into_inner()).0;
        }
        Ok(*outcome)
    }
}
impl RemoteActor {
    #[cfg(test)]
    pub(super) fn fixture()->Self {
        use crate::model::{BootId,HostSessionId,HostIncarnation,TerminalId};
        let process=Process::read(std::process::id() as i32).unwrap();
        let boot=BootId::new("fixture-boot");
        let resident=Resident {generation:1,session_id:None,title:None,ready:true,busy:false,waiting:false,working:false,activity:crate::agent::ActivityState::Idle,outcome:None,usage:Default::default(),native_continuity:Default::default(),requests:Vec::new(),completed:false,owns_work:true,idle:true,failure:None,confirmed_session:None,background_subagents:Vec::new(),interrupt_subagents:Vec::new(),account_reports:Vec::new(),next_input:1,outbox_ack:0,changed:1};
        Self(Arc::new(Remote {identity:ActorIdentity {instance:uuid::Uuid::new_v4(),record:Record {version:4,build:Some(registry::BuildIdentity::current(Kind::Native)),kind:Kind::Native,application:crate::instance::APPLICATION_ID.into(),boot:boot.clone(),terminal:TerminalId::new(),holder:process.clone(),root:process}},epoch:uuid::Uuid::new_v4(),proof:HostSessionProof {boot_id:boot,host_session_id:HostSessionId::new("fixture"),host_incarnation:HostIncarnation::new("fixture")},socket:PathBuf::from("/nonexistent/cagents-actor-fixture"),state:Mutex::new(Arc::new(resident)),environment:crate::terminal_ipc::ChatExecutionEnvironment(Default::default()),recovery_environment:Default::default(),input_sequence:Mutex::new(1),uncertain_input:Mutex::new(None),pending:Mutex::new(Vec::new()),notify:Arc::new(|_|{}),stop:AtomicBool::new(false),installed:AtomicBool::new(true),preserved:AtomicBool::new(false),worker:Mutex::new(None),startup:Mutex::new(None)}))
    }
    #[cfg(test)]
    pub(super) fn completed_failure_fixture()->Self {
        let actor=Self::fixture();
        {let mut state=actor.0.state.lock().unwrap();let state=Arc::make_mut(&mut state);state.completed=true;state.failure=Some("Provider cleanup is unconfirmed".into());state.owns_work=true;}
        actor
    }
    pub fn identity(&self)->ActorIdentity {self.0.identity.clone()}
    pub fn state(&self)->Arc<Resident> {self.0.state.lock().unwrap_or_else(|p|p.into_inner()).clone()}
    pub fn execution_environment(&self)->crate::terminal_ipc::ChatExecutionEnvironment {self.0.environment.clone()}
    pub fn recovery_environment_matches(&self,value:&BTreeMap<String,String>)->bool {&self.0.recovery_environment==value}
    #[cfg(test)]
    pub fn resource_lifetime(&self)->Box<dyn Fn()->bool> {let weak=Arc::downgrade(&self.0);Box::new(move||weak.upgrade().is_some())}
    pub fn spawn(mut spec:ProcessSpec,source:chat::Session,recovery_environment:BTreeMap<String,String>,notify:impl Fn(Update)+Send+Sync+'static)->Result<Self,String> {
        std::hint::black_box(&SERVE);
        if source.launch.provider!=crate::agent::Provider::Claude || !matches!(spec.transport,TransportSpec::JsonLines) {return Err("Retained actors require the Claude stream-json transport".into());}
        spec.remove_unset_environment();
        let proof=source.proof.clone().ok_or("Retained actor launch lacks daemon ownership proof")?;
        let directory=spec.execution.as_ref().and_then(|context|context.directory.clone());
        let directory_fd=directory.as_ref().map(|file|file.as_raw_fd());
        let request=Startup {protocol:PROTOCOL,source:source.clone(),program:spec.program,arguments:spec.arguments,environment:spec.environment,working_directory:spec.working_directory,directory_fd,directory_identity:spec.execution.as_ref().map(|context|context.directory_identity()).transpose()?.flatten(),recovery_environment};
        let (control,remote)=UnixStream::pair().map_err(|error|error.to_string())?;
        let fd=remote.as_raw_fd();
        let mut command=ChildCommand::new("/proc/self/exe");
        command.arg0("cagents-native").arg(ARGUMENT).arg(fd.to_string()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        unsafe {command.pre_exec(move||{crate::daemon::seal_inherited_descriptors()?;if libc::setsid()==-1||libc::fcntl(fd,libc::F_SETFD,0)==-1 {return Err(std::io::Error::last_os_error());}if let Some(directory)=directory_fd && libc::fcntl(directory,libc::F_SETFD,0)==-1 {return Err(std::io::Error::last_os_error());}Ok(())});}
        let owner=Arc::new(StartupOwner {control:Mutex::new(Some(control.try_clone().map_err(|error|error.to_string())?)),cancelled:AtomicBool::new(false),outcome:Mutex::new(chat::protocol::LaunchOutcome::NoProvider),finished:std::sync::Condvar::new()});
        let mut attempt=StartupAttempt {owner:owner.clone(),handed_off:false};
        if let Some(context)=&spec.execution {context.claim_owner(owner.clone());}
        // Allocate the sole child reaper before creating the helper. A startup
        // failure keeps an exact cleanup owner rather than killing an assumed
        // process group or allowing the provider to appear after cancellation.
        let (publish,incoming)=std::sync::mpsc::sync_channel::<std::process::Child>(1);
        let watcher=owner.clone();
        thread::Builder::new().name("cagents-actor-owner".into()).spawn(move||{
            if let Ok(mut child)=incoming.recv() {
                let proved=child.wait().is_ok_and(|status|status.code()==Some(73));
                if proved {*watcher.outcome.lock().unwrap_or_else(|p|p.into_inner())=chat::protocol::LaunchOutcome::TeardownProved;}
                watcher.finished.notify_all();
            }
        }).map_err(|error|error.to_string())?;
        {
            let mut outcome=owner.outcome.lock().unwrap_or_else(|p|p.into_inner());
            if owner.cancelled.load(Ordering::Acquire) {return Err("Retained actor launch was cancelled before process creation".into());}
            let child=command.spawn().map_err(|error|error.to_string())?;
            *outcome=chat::protocol::LaunchOutcome::Uncertain;
            publish.send(child).map_err(|_|"Retained actor startup lost its exact child reaper")?;
        }
        drop(remote);
        // Keep this channel through installation. EOF before commit asks the
        // helper to clean its still-private actor and every descendant.
        send(&control,&request,Duration::from_secs(30))?;
        let attached:Result<Attached,String>=receive(&control,Duration::from_secs(30))?;
        let attached=attached?;
        let handle=Self::from_attached(attached,proof,Some(control),notify)?;
        if let Some(context)=spec.execution {context.claim_owner(Arc::new(handle.clone()));}
        owner.control.lock().unwrap_or_else(|p|p.into_inner()).take();
        attempt.handed_off=true;
        Ok(handle)
    }
    pub fn inspect(record:&Record,proof:HostSessionProof)->Result<ActorInspection,String> {
        validate_record(record)?;
        let control=registry::connect(&registry::socket(&registry::directory(Kind::Native)?,record.terminal)).map_err(|error|error.to_string())?;
        verify_peer(&control,record)?;
        send(&control,&Invocation {proof,epoch:None,method:"inspect".into(),args:Value::Null},TIMEOUT)?;
        let response:Result<ActorInspection,String>=receive(&control,TIMEOUT)?;
        response
    }
    pub fn adopt(record:&Record,source:chat::Session,proof:HostSessionProof,notify:impl Fn(Update)+Send+Sync+'static)->Result<Self,String> {
        validate_record(record)?;
        let socket=registry::socket(&registry::directory(Kind::Native)?,record.terminal);
        let control=registry::connect(&socket).map_err(|error|error.to_string())?;
        verify_peer(&control,record)?;
        send(&control,&Invocation {proof:proof.clone(),epoch:None,method:"attach".into(),args:json!([source])},TIMEOUT)?;
        let attached:Result<Attached,String>=receive(&control,TIMEOUT)?;
        let attached=attached?;
        if &attached.identity.record!=record {return Err("Retained actor changed identity during attachment".into());}
        Self::from_attached(attached,proof,None,notify)
    }
    fn from_attached(attached:Attached,proof:HostSessionProof,startup:Option<UnixStream>,notify:impl Fn(Update)+Send+Sync+'static)->Result<Self,String> {
        if attached.protocol!=PROTOCOL {return Err("Retained Claude actor protocol is incompatible; no prompt was replayed".into());}
        validate_record(&attached.identity.record)?;
        let socket=registry::socket(&registry::directory(Kind::Native)?,attached.identity.record.terminal);
        let remote=Self(Arc::new(Remote {identity:attached.identity,epoch:attached.epoch,proof,socket,input_sequence:Mutex::new(attached.resident.next_input),uncertain_input:Mutex::new(None),state:Mutex::new(Arc::new(attached.resident)),environment:attached.execution_environment,recovery_environment:attached.recovery_environment,pending:Mutex::new(Vec::new()),notify:Arc::new(notify),stop:AtomicBool::new(false),installed:AtomicBool::new(false),preserved:AtomicBool::new(false),worker:Mutex::new(None),startup:Mutex::new(startup)}));
        let weak=Arc::downgrade(&remote.0);
        let worker=thread::Builder::new().name("cagents-actor-proxy".into()).spawn(move||{
            loop {
                let Some(inner)=weak.upgrade() else {break;};
                if inner.stop.load(Ordering::Acquire) {break;}
                let actor=Self(inner);
                let old=actor.state();
                let response=actor.call::<Option<Resident>>("state",json!([old.changed]));
                if let Err(error)=&response && actor.endpoint_failed(error) {actor.publish_unavailable(error);break;}
                if let Ok(Some(state))=response {
                    let changed=state.changed!=old.changed;
                    let generation=state.generation;
                    let activity=state.activity;
                    let outcome=state.outcome.clone();
                    let activity_changed=activity!=old.activity||outcome!=old.outcome;
                    let ready=state.ready&&!old.ready;
                    let usage:Vec<_>=state.account_reports.iter().filter(|report|!old.account_reports.contains(report)).cloned().collect();
                    *actor.0.state.lock().unwrap_or_else(|p|p.into_inner())=Arc::new(state);
                    if changed {(actor.0.notify)(Update::Observed {generation});(actor.0.notify)(Update::Presentation);}
                    if activity_changed {(actor.0.notify)(Update::Activity {state:activity,generation,outcome});}
                    if ready {(actor.0.notify)(Update::Ready);}
                    for report in usage {(actor.0.notify)(report);}
                }
                let identities:Vec<_>=actor.0.pending.lock().unwrap_or_else(|p|p.into_inner()).iter().filter(|pending|pending.reply.is_some()).map(|pending|pending.identity.clone()).collect();
                if !identities.is_empty() && let Ok(receipts)=actor.call::<Vec<InputReceipt>>("input_receipts",json!([identities])) {
                    actor.apply_receipts(&receipts);
                }
                drop(actor);
                thread::sleep(Duration::from_millis(50));
            }
        }).map_err(|error|error.to_string())?;
        *remote.0.worker.lock().unwrap_or_else(|p|p.into_inner())=Some(worker);
        Ok(remote)
    }
    fn endpoint_failed(&self,error:&str)->bool {
        terminal_endpoint_error(error,self.0.identity.record.holder.is_current())
    }
    fn publish_unavailable(&self,error:&str) {
        let dead=!self.0.identity.record.holder.is_current();
        let reason=if dead {"The retained Claude actor exited; exact provider cleanup and unfinished input outcomes are unconfirmed".to_owned()}else {format!("This retained actor attachment is no longer authoritative: {error}")};
        let generation;
        let outcome;
        {
            let mut state=self.0.state.lock().unwrap_or_else(|p|p.into_inner());
            let state=Arc::make_mut(&mut state);
            if dead && state.working {state.outcome=Some(crate::agent::ActivityOutcome::Interrupted {reason:reason.clone()});}
            state.failure=Some(reason.clone());state.ready=false;state.idle=false;state.busy=false;state.waiting=false;state.working=false;state.activity=crate::agent::ActivityState::Unknown;state.owns_work=true;
            generation=state.generation;outcome=state.outcome.clone();
        }
        for pending in &mut *self.0.pending.lock().unwrap_or_else(|p|p.into_inner()) {if let Some(reply)=pending.reply.take() {let _=reply.send(Err(format!("{reason}; input {} sequence {} remains unknown and must not be regenerated",pending.identity.id,pending.identity.sequence)));}}
        (self.0.notify)(Update::Activity {state:crate::agent::ActivityState::Unknown,generation,outcome});
        (self.0.notify)(Update::Observed {generation});(self.0.notify)(Update::Presentation);
        self.0.stop.store(true,Ordering::Release);
    }
    pub fn call<T:DeserializeOwned>(&self,method:&str,args:Value)->Result<T,String> {
        let control=registry::connect(&self.0.socket).map_err(|error|error.to_string())?;
        verify_peer(&control,&self.0.identity.record)?;
        send(&control,&Invocation {proof:self.0.proof.clone(),epoch:Some(self.0.epoch),method:method.into(),args},TIMEOUT)?;
        let response:Result<Value,String>=receive(&control,TIMEOUT)?;
        serde_json::from_value(response?).map_err(|error|error.to_string())
    }
    pub fn enqueue(&self,run:Option<crate::model::RunId>,command:Command)->Result<crossbeam_channel::Receiver<Result<(),String>>,String> {
        self.enqueue_payload(InputPayload::Command {run,command})
    }
    pub fn pending_inputs(&self)->Vec<InputIdentity> {self.0.pending.lock().unwrap_or_else(|p|p.into_inner()).iter().map(|pending|pending.identity.clone()).collect()}
    fn enqueue_payload(&self,payload:InputPayload)->Result<crossbeam_channel::Receiver<Result<(),String>>,String> {
        if self.0.stop.load(Ordering::Acquire) {return Err("Retained actor proxy is unavailable; no new input was admitted".into());}
        let payload=serde_json::to_value(payload).map_err(|error|error.to_string())?;
        let bytes=serde_json::to_vec(&payload).map_err(|error|error.to_string())?;
        let identity;
        let response;
        {
            let mut pending=self.0.pending.lock().unwrap_or_else(|p|p.into_inner());
            if pending.len()>=65 {return Err("Remote actor pending input capacity reached".into());}
            if let Some(identity)=&*self.0.uncertain_input.lock().unwrap_or_else(|p|p.into_inner()) {return Err(format!("Resolve retained input {} sequence {} before admitting another input",identity.id,identity.sequence));}
            let retained:usize=pending.iter().map(|pending|pending.bytes).try_fold(0usize,usize::checked_add).unwrap_or(usize::MAX);
            if retained.saturating_add(bytes.len())>64*1024*1024 {return Err("Remote actor pending input byte budget reached".into());}
            let mut sequence=self.0.input_sequence.lock().unwrap_or_else(|p|p.into_inner());
            identity=InputIdentity {sequence:*sequence,id:uuid::Uuid::new_v4(),hash:*blake3::hash(&bytes).as_bytes()};
            *sequence=sequence.checked_add(1).ok_or("Remote actor input sequence exhausted")?;
            let (reply,receiver)=crossbeam_channel::bounded(1);response=receiver;
            *self.0.uncertain_input.lock().unwrap_or_else(|p|p.into_inner())=Some(identity.clone());
            pending.push(Pending {identity:identity.clone(),payload:payload.clone(),bytes:bytes.len(),reply:Some(reply)});
        }
        // Preserve the exact identity and envelope before crossing the socket.
        // An ambiguous write keeps the receipt pending and fences new input.
        if let Ok(receipt)=self.call::<InputReceipt>("input",json!([identity,payload])) {self.apply_receipts(&[receipt]);}
        Ok(response)
    }
    /// Explicit retransmission of the exact protocol envelope. Existing helper
    /// receipts return the original outcome without invoking the adapter again.
    /// This never replays stdin or generates a replacement prompt UUID.
    pub fn retry_input(&self,identity:&InputIdentity)->Result<InputReceipt,String> {
        if self.0.stop.load(Ordering::Acquire) {return Err("Retained actor proxy is unavailable; original input was not replayed".into());}
        let payload=self.0.pending.lock().unwrap_or_else(|p|p.into_inner()).iter().find(|pending|&pending.identity==identity).map(|pending|pending.payload.clone()).ok_or("The original input envelope is unavailable; never reconstruct it")?;
        let receipt=self.call("input",json!([identity,payload]))?;
        self.apply_receipts(std::slice::from_ref(&receipt));Ok(receipt)
    }
    fn apply_receipts(&self,receipts:&[InputReceipt]) {
        let mut pending=self.0.pending.lock().unwrap_or_else(|p|p.into_inner());
        let mut uncertain=self.0.uncertain_input.lock().unwrap_or_else(|p|p.into_inner());
        pending.retain_mut(|pending| {
            let Some(receipt)=receipts.iter().find(|receipt|receipt.identity==pending.identity) else {return true;};
            if uncertain.as_ref()==Some(&receipt.identity) {*uncertain=None;}
            match &receipt.state {
                InputState::Acknowledged(result)=>{if let Some(reply)=pending.reply.take() {let _=reply.send(result.clone());}false},
                InputState::Unknown(error)=>{if let Some(reply)=pending.reply.take() {let _=reply.send(Err(error.clone()));}true},
                InputState::Accepted=>true,
            }
        });
    }

    pub fn submit(&self,run:Option<crate::model::RunId>,command:Command)->Result<(),Rejection> {
        let receiver=self.enqueue(run,command).map_err(Rejection::Unacknowledged)?;
        receiver.recv_timeout(Duration::from_secs(10)).map_err(|_|Rejection::Unacknowledged("Retained actor input acknowledgement is pending; use its stable receipt, never resend blindly".into()))?.map_err(Rejection::Refused)
    }
    pub fn interrupt(&self,run:Option<crate::model::RunId>,including_subagents:bool)->Result<(),String> {
        let receiver=self.enqueue_payload(InputPayload::Interrupt {run,including_subagents})?;
        receiver.recv_timeout(Duration::from_secs(10)).map_err(|_|"Retained interrupt acknowledgement remains unknown; never resend automatically".to_owned())?
    }

    pub fn commit_installation(&self) {if self.call::<()>("commit",Value::Null).is_ok() {self.0.installed.store(true,Ordering::Release);self.0.startup.lock().unwrap_or_else(|p|p.into_inner()).take();}}
    pub fn preserve(&self)->Result<(),String> {self.call::<()>("preserve",Value::Null)?;self.0.preserved.store(true,Ordering::Release);self.0.stop.store(true,Ordering::Release);Ok(())}
    pub fn close(&self)->Result<(),String> {self.call::<()>("close",Value::Null)?;self.0.stop.store(true,Ordering::Release);Ok(())}
}
impl chat::protocol::LaunchCleanup for RemoteActor {
    fn cleanup(&self)->Result<chat::protocol::LaunchOutcome,String> {self.close()?;Ok(chat::protocol::LaunchOutcome::TeardownProved)}
}
impl Drop for Remote {
    fn drop(&mut self) {
        self.stop.store(true,Ordering::Release);
        if let Some(worker)=self.worker.get_mut().unwrap_or_else(|p|p.into_inner()).take() && worker.thread().id()!=thread::current().id() {let _=worker.join();}
        // A committed proxy disappearing is a detach, including daemon death.
        // Startup EOF, held until commit, owns uninstalled launch rollback.
    }
}

struct Server {
    actor:LocalHandle,
    identity:ActorIdentity,
    source:chat::Session,
    lineage:Vec<chat::Session>,
    proof:HostSessionProof,
    epoch:uuid::Uuid,
    installed:bool,
    journal:Arc<Mutex<Journal>>,
    changed:Arc<std::sync::atomic::AtomicU64>,
    pending:Vec<(InputIdentity,crossbeam_channel::Receiver<Result<(),String>>)>,
    account_reports:Arc<Mutex<Vec<Update>>>,
    environment:crate::terminal_ipc::ChatExecutionEnvironment,
    recovery_environment:BTreeMap<String,String>,
    listener:UnixListener,
    socket:PathBuf,
    socket_identity:(u64,u64),
    _directory:Option<Arc<File>>,
}
impl Server {
    fn start(startup:Startup,control:&UnixStream)->Result<Self,String> {
        if startup.protocol!=PROTOCOL || startup.source.launch.provider!=crate::agent::Provider::Claude {return Err("Incompatible retained Claude actor launch".into());}
        let proof=startup.source.proof.clone().ok_or("Retained actor launch lacks daemon proof")?;
        let boot=crate::daemon::read_boot_id().map_err(|error|error.to_string())?;
        if proof.boot_id!=boot {return Err("Retained actor launch belongs to another boot".into());}
        let mut spec_and_adapter=chat::protocol::create(&startup.source.launch)?;
        if !matches!(spec_and_adapter.0.transport,TransportSpec::JsonLines) {return Err("Retained actor requires stream-json transport".into());}
        let directory=startup.directory_fd.map(|fd|unsafe {Arc::new(File::from_raw_fd(fd))});
        let mut execution=match startup.directory_identity {Some(identity)=>ExecutionContext::restored(identity),None=>ExecutionContext::new(None)};
        execution.directory=directory.clone();
        let spec=&mut spec_and_adapter.0;
        spec.program=startup.program;spec.arguments=startup.arguments;spec.environment=startup.environment;spec.working_directory=startup.working_directory;spec.unset_environment.clear();spec.execution=Some(Arc::new(execution));
        let environment=crate::terminal_ipc::ChatExecutionEnvironment(spec.environment.clone());
        let initial=spec_and_adapter.1.initialize()?;
        let mut initial=initial;
        let store=super::preflight(spec,&mut initial)?;
        let journal=Arc::new(Mutex::new(Journal::open(store.directory())?));
        let changed=Arc::new(std::sync::atomic::AtomicU64::new(1));
        let published=changed.clone();let outcomes=journal.clone();
        let account_reports=Arc::new(Mutex::new(Vec::<Update>::new()));let reported=account_reports.clone();
        if disconnected(control)? {return Err("Retained actor launch cancelled before process creation".into());}
        let actor=LocalHandle::start_inner(spec_and_adapter.0,spec_and_adapter.1,initial,store,startup.source.launch.session_id.clone(),None,move|update|{
            let mut journal=outcomes.lock().unwrap_or_else(|p|p.into_inner());
            if let Err(error)=journal.append(&update) {let _=journal.record_loss(&format!("Retained actor outcome continuity is unconfirmed: {error}"));}
            drop(journal);
            if let Update::AccountUsage {report,..}=&update {
                let mut reports=reported.lock().unwrap_or_else(|p|p.into_inner());
                reports.retain(|previous|!matches!(previous,Update::AccountUsage {report:previous,..} if std::mem::discriminant(previous)==std::mem::discriminant(report)));
                if reports.len()<16 {reports.push(update);}
            }
            published.fetch_add(1,Ordering::Release);
        })?;
        let provider=actor.provider_process().ok_or("Retained actor did not retain its exact provider process")?;
        let directory_path=registry::directory(Kind::Native)?;
        crate::daemon::prepare_private_directory(&directory_path).map_err(|error|error.to_string())?;
        let socket=registry::socket(&directory_path,startup.source.id);
        let listener=registry::bind(&socket).map_err(|error|error.to_string())?;
        let metadata=std::fs::symlink_metadata(&socket).map_err(|error|error.to_string())?;
        let socket_identity=(metadata.dev(),metadata.ino());
        listener.set_nonblocking(true).map_err(|error|error.to_string())?;
        let mut build=registry::BuildIdentity::current(Kind::Native);build.capabilities.push(CAPABILITY.into());
        let identity=ActorIdentity {instance:uuid::Uuid::new_v4(),record:Record {version:4,build:Some(build),kind:Kind::Native,application:crate::instance::APPLICATION_ID.into(),boot,terminal:startup.source.id,holder:Process::read(std::process::id() as i32).map_err(|error|error.to_string())?,root:provider}};
        identity.record.create(&directory_path).map_err(|error|error.to_string())?;
        Ok(Self {actor,identity,source:startup.source,lineage:Vec::new(),proof,epoch:uuid::Uuid::new_v4(),installed:false,journal,changed,pending:Vec::new(),account_reports,environment,recovery_environment:startup.recovery_environment,listener,socket,socket_identity,_directory:directory})
    }
    fn resident(&self)->Result<Resident,String> {
        // Clone resident metadata only. Do not hydrate the Store or serialize
        // any growing parent/child tool body to service periodic observation.
        let state=self.actor.0.state.lock().unwrap_or_else(|p|p.into_inner());
        let generation=state.generation;let session_id=state.session_id.clone();let title=state.title.clone();let ready=state.ready;let busy=state.busy;let waiting=state.waiting;let working=state.is_working();let activity=state.activity();let outcome=state.outcome.clone();let usage=state.usage.clone();let requests=state.requests.clone();
        drop(state);
        let journal=self.journal.lock().unwrap_or_else(|p|p.into_inner());
        let failure=journal.loss()?.or_else(||self.actor.failure());
        Ok(Resident {generation,session_id,title,ready,busy,waiting,working,activity,outcome,usage,requests,native_continuity:chat::NativeContinuity::Durable {provider_version:"claude-stream-json/actor-v1".into()},owns_work:self.actor.owns_work()||failure.is_some(),completed:self.actor.is_completed(),idle:self.actor.is_idle(),failure,confirmed_session:self.actor.confirmed_session_id(),background_subagents:self.actor.background_subagents(),interrupt_subagents:self.actor.interrupt_subagents(),account_reports:self.account_reports.lock().unwrap_or_else(|p|p.into_inner()).clone(),next_input:journal.next_input()?,outbox_ack:journal.acknowledged()?,changed:self.changed.load(Ordering::Acquire)})
    }

    fn attached(&self)->Result<Attached,String> {Ok(Attached {protocol:PROTOCOL,identity:self.identity.clone(),epoch:self.epoch,resident:self.resident()?,source:self.source.clone(),execution_environment:self.environment.clone(),recovery_environment:self.recovery_environment.clone()})}
    fn finish_inputs(&mut self) {
        let journal=self.journal.clone();
        self.pending.retain(|(identity,response)| {
            let result=match response.try_recv() {Ok(result)=>Some(InputState::Acknowledged(result)),Err(crossbeam_channel::TryRecvError::Disconnected)=>Some(InputState::Unknown("The retained actor stopped before acknowledging this input; never replay it automatically".into())),Err(crossbeam_channel::TryRecvError::Empty)=>None};
            if let Some(state)=result {
                let mut journal=journal.lock().unwrap_or_else(|p|p.into_inner());
                if let Err(error)=journal.finish(identity,state) {let _=journal.record_loss(&format!("Cannot persist retained input outcome: {error}"));}
                false
            } else {true}
        });
    }
    fn invoke(&mut self,request:Invocation)->Result<Value,String> {
        if request.method=="inspect" {return serde_json::to_value(ActorInspection {identity:self.identity.clone(),source:self.source.clone(),previous_sources:self.lineage.clone(),state:self.resident()?,execution_environment:self.environment.clone(),recovery_environment:self.recovery_environment.clone()}).map_err(|error|error.to_string());}
        if request.method=="attach" {
            let (source,):(chat::Session,)=serde_json::from_value(request.args).map_err(|error|error.to_string())?;
            if source!=self.source && !self.lineage.contains(&source) {return Err("Retained actor source/account identity does not match this chat".into());}
            self.epoch=uuid::Uuid::new_v4();self.proof=request.proof;
            return serde_json::to_value(self.attached()?).map_err(|error|error.to_string());
        }
        if request.epoch!=Some(self.epoch) || request.proof!=self.proof {return Err("Retained actor attachment was superseded; stale input and ACKs are refused".into());}
        macro_rules! decode {($type:ty)=>{serde_json::from_value::<$type>(request.args).map_err(|error|error.to_string())?};}
        macro_rules! value {($expression:expr)=>{serde_json::to_value($expression).map_err(|error|error.to_string())};}
        match request.method.as_str() {
            "state"=>{let (after,)=decode!((u64,));if after==self.changed.load(Ordering::Acquire) {Ok(Value::Null)}else {value!(Some(self.resident()?))}},
            "outbox"=>{let (after,)=decode!((u64,));value!(self.journal.lock().unwrap_or_else(|p|p.into_inner()).page(after)?)},
            "ack_outbox"=>{let (through,)=decode!((u64,));self.journal.lock().unwrap_or_else(|p|p.into_inner()).ack(through)?;Ok(Value::Null)},
            "input_receipt"=>{let (identity,)=decode!((InputIdentity,));value!(self.journal.lock().unwrap_or_else(|p|p.into_inner()).input(&identity)?)},
            "input_receipts"=>{let (identities,)=decode!((Vec<InputIdentity>,));if identities.len()>65 {return Err("Input receipt batch exceeds capacity".into());}let journal=self.journal.lock().unwrap_or_else(|p|p.into_inner());let mut receipts=Vec::new();for identity in identities {if let Some(receipt)=journal.input(&identity)? {receipts.push(receipt);}}value!(receipts)},
            "ack_inputs"=>{let (through,)=decode!((u64,));self.journal.lock().unwrap_or_else(|p|p.into_inner()).retire_inputs(through)?;Ok(Value::Null)},
            "input"=>{
                let (identity,payload)=decode!((InputIdentity,Value));
                if *blake3::hash(&serde_json::to_vec(&payload).map_err(|error|error.to_string())?).as_bytes()!=identity.hash {return Err("Retained input hash differs from its envelope".into());}
                let mut journal=self.journal.lock().unwrap_or_else(|p|p.into_inner());
                if let Some(receipt)=journal.input(&identity)? {return value!(receipt);}
                if self.pending.len()>=65 {return Err("Retained actor input completion capacity reached".into());}
                let command=match serde_json::from_value::<InputPayload>(payload).map_err(|error|error.to_string())? {InputPayload::Command {run,command}=>super::ActorCommand::Provider {run,command},InputPayload::Interrupt {run,including_subagents}=>super::ActorCommand::Interrupt {run,including_subagents}};
                let receipt=journal.admit(&identity)?;
                match self.actor.enqueue_actor(command) {Ok(reply)=>self.pending.push((identity,reply)),Err(error)=>{journal.finish(&identity,InputState::Acknowledged(Err(error)))?;}}
                value!(receipt)
            },
            "snapshot"=>{let (after,)=decode!((Option<u64>,));value!(self.actor.snapshot(after))},
            "presentation"=>{let (id,after)=decode!((crate::model::TerminalId,Option<u64>));if id!=self.source.id {return Err("Retained presentation names another chat".into());}value!(self.actor.presentation(id,after))},
            "file_query"=>{let (command,)=decode!((Command,));value!(self.actor.file_query(&command)?)},
            "exit_output"=>{let (id,offset)=decode!((String,u64));value!(self.actor.provider_exit_output(&id,offset)?)},
            "visible"=>{let (visible,)=decode!((bool,));self.actor.set_visible(visible);Ok(Value::Null)},
            "observe"=>{self.actor.observe_next_update();Ok(Value::Null)},
            "readiness"=>value!(self.actor.check_update_readiness()?),
            "commit"=>{self.installed=true;Ok(Value::Null)},
            "preserve"=>{if !self.installed {return Err("Cannot preserve an uninstalled actor".into());}Ok(Value::Null)},
            "update_source"=>{
                let (source,console,pair)=decode!((chat::Session,Option<crate::model::LaunchRecipe>,Option<crate::terminal::holder::Record>));
                if console.is_some() || pair.is_some() {return Err("Claude actors cannot acquire a Codex console pairing".into());}
                let mut expected=self.source.launch.clone();expected.session_id=self.actor.confirmed_session_id().or(expected.session_id);chat::protocol::align_resume_identity(&mut expected)?;
                if source.id!=self.source.id || source.launch!=expected || source.proof.as_ref()!=Some(&self.proof) {return Err("Retained source update changed provider/account/launch identity".into());}
                let retained=serde_json::to_vec(&self.lineage).map_err(|error|error.to_string())?.len();
                if self.lineage.len()>=1024 || retained+serde_json::to_vec(&self.source).map_err(|error|error.to_string())?.len()>4*1024*1024 {return Err("Retained source lineage capacity reached".into());}
                if source!=self.source {self.lineage.push(self.source.clone());self.source=source;}
                Ok(Value::Null)
            },
            "close_if_idle"=>{let (generation,)=decode!((u64,));self.actor.close_if_idle(generation)?;reap_orphans()?;self.publish_close()?;Ok(Value::Null)},
            "close"=>{self.actor.close()?;reap_orphans()?;self.publish_close()?;Ok(Value::Null)},
            _=>Err("Unknown retained actor method/version".into()),
        }
    }
    fn publish_close(&self)->Result<(),String> {chat::native_holder::publish_actor_close(&self.identity.record,&self.proof,&self.source,&self.lineage)}
    fn serve(mut self,startup:UnixStream)->Result<(),String> {
        send(&startup,&Ok::<_,String>(self.attached()?),Duration::from_secs(30))?;
        loop {
            self.finish_inputs();
            if !self.installed && disconnected(&startup)? {self.actor.close()?;reap_orphans()?;break;}
            match self.listener.accept() {
                Ok((control,_))=>{
                    let response=(|| {
                        let request:Invocation=receive(&control,TIMEOUT)?;
                        authorize(&control,&request.proof)?;
                        let close=matches!(request.method.as_str(),"close"|"close_if_idle");
                        let result=self.invoke(request);
                        let finished=close && result.is_ok();
                        send(&control,&result,TIMEOUT)?;
                        Ok::<_,String>(finished)
                    })();
                    if matches!(response,Ok(true)) {break;}
                },
                Err(error) if error.kind()==std::io::ErrorKind::WouldBlock=>thread::sleep(Duration::from_millis(20)),
                Err(error) if error.kind()==std::io::ErrorKind::Interrupted=>{},
                Err(error)=>return Err(error.to_string()),
            }
        }
        if std::fs::symlink_metadata(&self.socket).is_ok_and(|metadata|(metadata.dev(),metadata.ino())==self.socket_identity) {std::fs::remove_file(&self.socket).map_err(|error|error.to_string())?;}
        self.identity.record.remove_record(&registry::directory(Kind::Native)?).map_err(|error|error.to_string())?;
        Ok(())
    }
}

fn validate_record(record:&Record)->Result<(),String> {
    if record.kind!=Kind::Native || record.application!=crate::instance::APPLICATION_ID || !record.holder.is_current() || record.boot!=crate::daemon::read_boot_id().map_err(|error|error.to_string())? || !record.build.as_ref().is_some_and(|build|build.capabilities.iter().any(|capability|capability==CAPABILITY)) {return Err("Native holder is incompatible with retained Claude actor protocol; no input was replayed".into());}
    Ok(())
}
fn verify_peer(control:&UnixStream,record:&Record)->Result<(),String> {
    let credentials=peer(control)?;
    if credentials.uid!=crate::daemon::current_uid() || credentials.pid!=record.holder.pid || !record.holder.is_current() {return Err("Retained actor socket peer identity changed".into());}Ok(())
}
fn authorize(control:&UnixStream,proof:&HostSessionProof)->Result<(),String> {
    let credentials=peer(control)?;
    let paths=crate::daemon::RuntimePaths::discover().map_err(|error|error.to_string())?;
    let owner=crate::daemon::lease_owner(&paths.lock,proof).map_err(|error|error.to_string())?;
    if credentials.uid!=crate::daemon::current_uid() || u32::try_from(credentials.pid).ok()!=Some(owner) {return Err("Retained actor caller does not own the current daemon lease".into());}Ok(())
}
fn peer(control:&UnixStream)->Result<libc::ucred,String> {
    let mut credentials:libc::ucred=unsafe {std::mem::zeroed()};let mut length=std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {libc::getsockopt(control.as_raw_fd(),libc::SOL_SOCKET,libc::SO_PEERCRED,(&raw mut credentials).cast(),&mut length)}!=0 {return Err(std::io::Error::last_os_error().to_string());}Ok(credentials)
}
fn send(control:&UnixStream,value:&impl Serialize,timeout:Duration)->Result<(),String> {
    let bytes=serde_json::to_vec(value).map_err(|error|error.to_string())?;
    if bytes.len()>MAX_FRAME {return Err("Retained actor frame exceeds the bounded protocol capacity".into());}
    control.set_write_timeout(Some(timeout)).map_err(|error|error.to_string())?;
    let mut writer=control;writer.write_all(&(bytes.len() as u32).to_le_bytes()).and_then(|()|writer.write_all(&bytes)).map_err(|error|error.to_string())
}
fn receive<T:DeserializeOwned>(control:&UnixStream,timeout:Duration)->Result<T,String> {
    control.set_read_timeout(Some(timeout)).map_err(|error|error.to_string())?;
    let mut reader=control;let mut size=[0;4];reader.read_exact(&mut size).map_err(|error|error.to_string())?;
    let size=u32::from_le_bytes(size) as usize;if size>MAX_FRAME {return Err("Retained actor frame exceeds the bounded protocol capacity".into());}
    let mut bytes=vec![0;size];reader.read_exact(&mut bytes).map_err(|error|error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error|error.to_string())
}
fn disconnected(control:&UnixStream)->Result<bool,String> {
    let mut event=libc::pollfd {fd:control.as_raw_fd(),events:libc::POLLHUP|libc::POLLERR,revents:0};
    if unsafe {libc::poll(&mut event,1,0)}<0 {return Err(std::io::Error::last_os_error().to_string());}
    Ok(event.revents&(libc::POLLHUP|libc::POLLERR)!=0)
}
/// Only after the actor's ChildGuard has joined: no competing provider reaper
/// remains. A child PID cannot be recycled until this sole reaper consumes it.
fn reap_orphans()->Result<(),String> {
    let deadline=Instant::now()+Duration::from_secs(5);
    loop {
        let children=std::fs::read_to_string(format!("/proc/self/task/{}/children",std::process::id())).map_err(|error|error.to_string())?;
        for pid in children.split_whitespace().filter_map(|pid|pid.parse::<i32>().ok()) {unsafe {libc::kill(pid,libc::SIGKILL);}}
        loop {match unsafe {libc::waitpid(-1,std::ptr::null_mut(),libc::WNOHANG)} {pid if pid>0=>continue,0=>break,_ if std::io::Error::last_os_error().raw_os_error()==Some(libc::ECHILD)=>return Ok(()),_ if std::io::Error::last_os_error().raw_os_error()==Some(libc::EINTR)=>continue,_=>return Err(std::io::Error::last_os_error().to_string())}}
        if Instant::now()>=deadline {return Err("Retained actor descendants have not finished cleanup; ownership is retained".into());}
        thread::sleep(Duration::from_millis(10));
    }
}
#[used]
#[unsafe(link_section=".init_array")]
static SERVE:extern "C" fn()=serve_if_requested;
extern "C" fn serve_if_requested() {
    let Ok(arguments)=std::fs::read("/proc/self/cmdline") else {return;};
    let mut arguments=arguments.split(|byte|*byte==0).skip(1);
    if arguments.next()!=Some(ARGUMENT.as_bytes()) {return;}
    let fd=arguments.next().and_then(|fd|std::str::from_utf8(fd).ok()?.parse::<RawFd>().ok()).filter(|fd|*fd>2);
    let Some(fd)=fd else {std::process::exit(2);};
    unsafe {libc::signal(libc::SIGPIPE,libc::SIG_IGN);libc::signal(libc::SIGCHLD,libc::SIG_DFL);libc::prctl(libc::PR_SET_NAME,c"cagents-native".as_ptr());}
    if unsafe {libc::prctl(libc::PR_SET_CHILD_SUBREAPER,1)}!=0 {std::process::exit(2);}
    let control=unsafe {UnixStream::from_raw_fd(fd)};
    let result=(|| {let startup:Startup=receive(&control,Duration::from_secs(30))?;authorize(&control,startup.source.proof.as_ref().ok_or("Retained startup lacks proof")?)?;let _=crate::daemon::process_scope::enroll_holder();let server=Server::start(startup,&control)?;server.serve(control.try_clone().map_err(|error|error.to_string())?)})();
    if let Err(error)=result {let _=send(&control,&Err::<Attached,_>(error),TIMEOUT);}
    while reap_orphans().is_err() {thread::sleep(Duration::from_millis(100));}
    std::process::exit(73);
}

fn terminal_endpoint_error(error:&str,holder_current:bool)->bool {
    !holder_current || error.contains("attachment was superseded") || error.contains("does not own the current daemon lease")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lost_input_reply_keeps_the_original_envelope_and_fences_new_input() {
        let actor=RemoteActor::fixture();
        let response=actor.enqueue(None,Command::Interrupt).unwrap();
        let identity=actor.pending_inputs().pop().unwrap();
        assert_eq!(identity.sequence,1);
        assert!(actor.enqueue(None,Command::Interrupt).is_err());
        assert_eq!(actor.pending_inputs(),vec![identity.clone()]);
        let directory=tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal=Journal::open(directory.path()).unwrap();
        let receipt=journal.admit(&identity).unwrap();
        assert_eq!(journal.admit(&identity).unwrap().identity,identity);
        actor.apply_receipts(&[receipt]);
        assert!(actor.0.uncertain_input.lock().unwrap().is_none());
        journal.finish(&identity,InputState::Acknowledged(Ok(()))).unwrap();
        actor.apply_receipts(&[journal.input(&identity).unwrap().unwrap()]);
        assert_eq!(response.try_recv(),Ok(Ok(())));
        assert!(actor.pending_inputs().is_empty());
        assert_eq!(*actor.0.input_sequence.lock().unwrap(),2);
    }
    #[test]
    fn unavailable_proxy_settles_pending_input_and_stops_reporting_healthy_metadata() {
        let actor=RemoteActor::fixture();
        let response=actor.enqueue(None,Command::Interrupt).unwrap();
        actor.publish_unavailable("Retained actor attachment was superseded");
        let state=actor.state();
        assert!(!state.ready);assert!(!state.idle);assert!(state.failure.is_some());
        assert_eq!(state.activity,crate::agent::ActivityState::Unknown);
        assert!(state.owns_work,"disconnection is not provider cleanup proof");
        assert!(response.try_recv().unwrap().unwrap_err().contains("must not be regenerated"));
        assert!(state.outcome.is_none(),"a superseded attachment does not prove provider interruption");
    }
    #[test]
    fn connection_loss_requires_death_or_positive_authority_loss_evidence() {
        assert!(terminal_endpoint_error("connection reset",false));
        assert!(terminal_endpoint_error("Retained actor attachment was superseded; stale input and ACKs are refused",true));
        assert!(terminal_endpoint_error("Retained actor caller does not own the current daemon lease",true));
        assert!(!terminal_endpoint_error("Resource temporarily unavailable (os error 11)",true));
        assert!(!terminal_endpoint_error("timed out",true));
    }
}
