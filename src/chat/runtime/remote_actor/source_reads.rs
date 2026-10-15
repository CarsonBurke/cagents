//! Typed original-source reads use the actual ingestion Store in the helper.
//! Dependencies: cli_contracts ee6993844, Changes source2caedaf38 and raw
//! parent source2d8cd3a38 (including its privacy/publication follow-ups).
//! One fixed worker owns disk reads and socket replies. A stalled cache never
//! occupies the helper's control loop or creates per-request threads.
use super::*;
use crate::chat::cache::{SourceError, SourcePage, SourceQuery,changes::{ManifestQuery,Manifest,PatchQuery,PatchPage}};
use crate::chat::cache::raw::{RawSourceQuery,RawSourcePage};
use std::sync::atomic::AtomicU64;

const QUEUED_READS:usize=4;
enum Query {
    Text(SourceQuery),Raw(RawSourceQuery),Manifest(ManifestQuery),Patch(PatchQuery),
    Snapshot(Option<u64>),
    Presentation(crate::model::TerminalId,Option<u64>),
    Files(Command),ExitOutput(String,u64),
}
struct Job {
    actor:LocalHandle,query:Query,control:UnixStream,proof:HostSessionProof,
    generation:u64,deadline:Instant,journal:Arc<Mutex<Journal>>,
}
pub(super) struct Service {
    send:crossbeam_channel::Sender<Job>,generation:Arc<AtomicU64>,
}
impl Service {
    fn new()->Result<Self,String> {
        let (send,receive)=crossbeam_channel::bounded::<Job>(QUEUED_READS);
        let generation=Arc::new(AtomicU64::new(1));let current=generation.clone();
        // Deliberately detach the sole owned worker: dropping a reader never
        // joins disk IO or TLS cleanup on the helper control thread.
        thread::Builder::new().name("retained-source-reader".into()).spawn(move|| {
            while let Ok(job)=receive.recv() {
                let response=(|| {
                    if Instant::now()>=job.deadline {return Err("Original-source read budget expired".into());}
                    if current.load(Ordering::Acquire)!=job.generation {return Err("Source reader attachment changed".into());}
                    authorize(&job.control,&job.proof)?;
                    let result=match job.query {
                        Query::Text(query)=>serde_json::to_value(job.actor.source_query(query)),
                        Query::Raw(query)=>serde_json::to_value(job.actor.raw_source_query(query)),
                        Query::Manifest(query)=>serde_json::to_value(job.actor.changes_manifest(query)),
                        Query::Patch(query)=>serde_json::to_value(job.actor.change_patch(query)),
                        Query::Snapshot(after)=>{
                            let mut snapshot=job.actor.snapshot(after);
                            if let Some(snapshot)=&mut snapshot
                                && let Some(loss)=job.journal.lock().unwrap_or_else(|p|p.into_inner()).continuity_loss()? {
                                snapshot.error=Some(loss.reason.clone());
                                snapshot.outcome=Some(crate::agent::ActivityOutcome::Interrupted {reason:loss.reason});
                            }
                            serde_json::to_value(snapshot)
                        },
                        Query::Presentation(id,after)=>serde_json::to_value(job.actor.presentation(id,after)),
                        Query::Files(command)=>serde_json::to_value(job.actor.file_query(&command)?),
                        Query::ExitOutput(id,offset)=>serde_json::to_value(job.actor.provider_exit_output(&id,offset)?),
                    }.map_err(|error|error.to_string())?;
                    if current.load(Ordering::Acquire)!=job.generation {return Err("Source reader attachment changed".into());}
                    authorize(&job.control,&job.proof)?;
                    Ok::<_,String>(result)
                })();
                let _=deadline_io::send_until(&job.control,&response,job.deadline);
            }
        }).map_err(|error|error.to_string())?;
        Ok(Self {send,generation})
    }
    pub(super) fn invalidate(&self) {
        // Zero permanently refuses new jobs if the finite counter is exhausted.
        let _=self.generation.fetch_update(Ordering::AcqRel,Ordering::Acquire,|old|Some(if old==0 {0}else{old.checked_add(1).unwrap_or(0)}));
    }
}
pub(super) fn is_read(method:&str)->bool {
    matches!(method,"source_query"|"raw_source_query"|"changes_manifest"|"change_patch"|"snapshot"|"presentation"|"file_query"|"exit_output")
}
impl Server {
    pub(super) fn dispatch_source_read(&mut self,control:UnixStream,request:Invocation)->Result<(),(UnixStream,String)> {
        let prepared=(|| {
            if request.epoch!=Some(self.epoch) || request.proof!=self.proof {return Err("Retained actor attachment was superseded; source read refused".into());}
            let query=match request.method.as_str() {
                "source_query"=>Query::Text(serde_json::from_value(request.args).map_err(|error|error.to_string())?),
                "raw_source_query"=>Query::Raw(serde_json::from_value(request.args).map_err(|error|error.to_string())?),
                "changes_manifest"=>Query::Manifest(serde_json::from_value(request.args).map_err(|error|error.to_string())?),
                "change_patch"=>Query::Patch(serde_json::from_value(request.args).map_err(|error|error.to_string())?),
                "snapshot"=>{
                    let (after,)=serde_json::from_value(request.args).map_err(|error|error.to_string())?;
                    Query::Snapshot(after)
                },
                "presentation"=>{
                    let (id,after)=serde_json::from_value(request.args).map_err(|error|error.to_string())?;
                    if id!=self.source.id {return Err("Retained presentation names another chat".into());}
                    Query::Presentation(id,after)
                },
                "file_query"=>{
                    let (command,)=serde_json::from_value(request.args).map_err(|error|error.to_string())?;
                    Query::Files(command)
                },
                "exit_output"=>{
                    let (id,offset)=serde_json::from_value(request.args).map_err(|error|error.to_string())?;
                    Query::ExitOutput(id,offset)
                },
                _=>return Err("Unknown source reader method".into()),
            };
            if self.source_reads.is_none() {self.source_reads=Some(Service::new()?);}
            Ok::<_,String>(query)
        })();
        let query=match prepared {Ok(query)=>query,Err(error)=>return Err((control,error))};
        let reads=self.source_reads.as_ref().unwrap();let generation=reads.generation.load(Ordering::Acquire);
        if generation==0 {return Err((control,"Source reader attachment capacity exhausted".into()));}
        let job=Job {actor:self.actor.clone(),query,control,proof:self.proof.clone(),generation,deadline:Instant::now()+TIMEOUT,journal:self.journal.clone()};
        reads.send.try_send(job).map_err(|error| {
            let message=match &error {crossbeam_channel::TrySendError::Full(_)=>"Original-source reader capacity reached",crossbeam_channel::TrySendError::Disconnected(_)=>"Original-source reader is unavailable"};
            (error.into_inner().control,message.into())
        })
    }
}

impl RemoteActor {
    pub fn source_query(&self, query:SourceQuery)->Result<SourcePage,SourceError> {
        self.source_query_until(query,Instant::now()+TIMEOUT)
    }
    pub fn changes_manifest(&self,query:ManifestQuery)->Result<Manifest,SourceError> {
        let args=serde_json::to_value(query).map_err(|error|SourceError::Transport(error.to_string()))?;
        self.call_until::<Result<Manifest,SourceError>>("changes_manifest",args,Instant::now()+TIMEOUT).map_err(SourceError::Transport)?
    }
    pub fn raw_source_query(&self,query:RawSourceQuery)->Result<RawSourcePage,SourceError> {
        self.raw_source_query_until(query,Instant::now()+TIMEOUT)
    }
    fn raw_source_query_until(&self,query:RawSourceQuery,deadline:Instant)->Result<RawSourcePage,SourceError> {
        let args=serde_json::to_value(query).map_err(|error|SourceError::Transport(error.to_string()))?;
        self.call_until::<Result<RawSourcePage,SourceError>>("raw_source_query",args,deadline).map_err(SourceError::Transport)?
    }
    pub fn change_patch(&self,query:PatchQuery)->Result<PatchPage,SourceError> {
        let args=serde_json::to_value(query).map_err(|error|SourceError::Transport(error.to_string()))?;
        self.call_until::<Result<PatchPage,SourceError>>("change_patch",args,Instant::now()+TIMEOUT).map_err(SourceError::Transport)?
    }
    fn source_query_until(&self,query:SourceQuery,deadline:Instant)->Result<SourcePage,SourceError> {
        let args=serde_json::to_value(query).map_err(|error|SourceError::Transport(error.to_string()))?;
        self.call_until::<Result<SourcePage,SourceError>>("source_query",args,deadline)
            .map_err(SourceError::Transport)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::cache::{SourceEpoch,SourceKey,SourceMetadata};
    use crate::chat::cache::raw::{RawSourceField,RawSourceMetadata,RawRepresentation,RawToolCapture};
    fn raw_query()->RawSourceQuery {
        RawSourceQuery {source:SourceKey {agent_id:None,source_id:"exact-raw-call".into()},
            field:RawSourceField::ToolInput,offset:0,limit:256,expected_epoch:None,until:None}
    }
    #[test]
    fn original_call_reads_use_the_same_bounded_worker_and_leave_stop_admission_live() {
        const CHILD:&str="CAGENTS_RAW_SOURCE_READER_CONTROL_TEST";
        if std::env::var_os(CHILD).is_none() {
            let root=tempfile::tempdir_in("/var/tmp").unwrap();let test=thread::current().name().unwrap().to_owned();
            let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact",&test,"--nocapture"])
                .env(CHILD,"1").env("XDG_RUNTIME_DIR",root.path()).env("XDG_DATA_HOME",root.path().join("data"))
                .env("XDG_CONFIG_HOME",root.path().join("config")).env("XDG_CACHE_HOME",root.path().join("cache"))
                .env("HOME",root.path().join("home")).env("CAGENTS_LOCAL_DEBUG","0").output().unwrap();
            assert!(output.status.success(),"stdout={} stderr={}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));return;
        }
        for reattach in [false,true] {
            let mut fixture=super::super::tests::resident_fixture();let server=&mut fixture.server;
            server.proof.boot_id=crate::daemon::read_boot_id().unwrap();server.source.proof=Some(server.proof.clone());
            let paths=crate::daemon::RuntimePaths::discover().unwrap();let _lease=crate::daemon::HostLease::acquire(&paths,&server.proof).unwrap();
            let store=server.actor.0.store.clone();let mut held=store.lock().unwrap();
            held.apply(chat::Event::RawToolCapture(RawToolCapture::Full {source:raw_query().source,
                name:"Opaque".into(),input:"{\"original\":true}".into(),command:None})).unwrap();
            let (client,control)=UnixStream::pair().unwrap();
            let request=Invocation {proof:server.proof.clone(),epoch:Some(server.epoch),method:"raw_source_query".into(),args:serde_json::to_value(raw_query()).unwrap()};
            assert!(is_read(&request.method));assert!(server.dispatch_source_read(control,request).is_ok());
            let started=Instant::now();
            let state=server.invoke(Invocation {proof:server.proof.clone(),epoch:Some(server.epoch),method:"state".into(),args:json!([0])}).unwrap();
            assert!(!state.is_null());assert!(started.elapsed()<Duration::from_secs(1));
            let fallback=server.invoke(Invocation {proof:server.proof.clone(),epoch:Some(server.epoch),method:"raw_source_query".into(),args:Value::Null}).unwrap_err();
            assert!(fallback.contains("off-loop reader"));
            if reattach {
                let source=server.source.clone();server.invoke(Invocation {proof:server.proof.clone(),epoch:None,method:"attach".into(),args:json!([source])}).unwrap();
            } else {
                let id=server.identity.instance;
                server.invoke(Invocation {proof:server.proof.clone(),epoch:Some(server.epoch),method:"fence_session_stop".into(),args:json!([id,500])}).unwrap();
                assert!(server.actor.0.stop.load(Ordering::Acquire));
            }
            drop(held);
            let result:Result<Result<RawSourcePage,SourceError>,String>=receive(&client,TIMEOUT).unwrap();
            if reattach {assert!(result.unwrap_err().contains("attachment changed"));} else {
                let page=result.unwrap().unwrap();assert_eq!(page.text,"{\"original\":true}");
                assert!(page.metadata.complete_call);assert_eq!(page.metadata.representation,RawRepresentation::CanonicalJson);
            }
        }
    }
    #[test]
    fn authenticated_original_call_pages_keep_representation_incarnation_and_typed_refusals() {
        for changed in [false,true] {
            let root=tempfile::tempdir_in("/var/tmp").unwrap();let socket=root.path().join("raw-reader.sock");let listener=UnixListener::bind(&socket).unwrap();
            let mut actor=RemoteActor::fixture();Arc::get_mut(&mut actor.0).unwrap().socket=socket;
            let mut query=raw_query();query.offset=4;query.until=Some(8);query.expected_epoch=Some(SourceEpoch {incarnation:uuid::Uuid::new_v4(),revision:7});
            let expected=query.clone();let epoch=actor.0.epoch;let proof=actor.0.proof.clone();
            let worker=thread::spawn(move|| {
                let (control,_)=listener.accept().unwrap();let request:Invocation=receive(&control,TIMEOUT).unwrap();
                assert_eq!(request.method,"raw_source_query");assert_eq!(request.epoch,Some(epoch));assert_eq!(request.proof,proof);
                assert_eq!(serde_json::from_value::<RawSourceQuery>(request.args).unwrap(),expected);
                let page=if changed {Err(SourceError::Changed)} else {Ok(RawSourcePage {source:expected.source,field:expected.field,
                    epoch:expected.expected_epoch.unwrap(),source_len:8,offset:4,next_offset:8,text:"null".into(),complete:true,
                    metadata:RawSourceMetadata {tool_name:"Opaque".into(),representation:RawRepresentation::StreamJson,complete_call:false}})};
                send(&control,&Ok::<_,String>(page),TIMEOUT).unwrap();
            });
            let result=actor.raw_source_query(query.clone());worker.join().unwrap();
            if changed {assert_eq!(result,Err(SourceError::Changed));} else {
                let page=result.unwrap();assert_eq!(Some(page.epoch),query.expected_epoch);assert_eq!(page.source,query.source);
                assert_eq!(page.metadata.representation,RawRepresentation::StreamJson);assert!(!page.metadata.complete_call);
            }
            assert!(actor.pending_inputs().is_empty());
        }
    }
    #[test]
    fn stalled_original_call_read_uses_one_absolute_budget() {
        let root=tempfile::tempdir_in("/var/tmp").unwrap();let socket=root.path().join("stalled-raw.sock");let listener=UnixListener::bind(&socket).unwrap();
        let mut actor=RemoteActor::fixture();Arc::get_mut(&mut actor.0).unwrap().socket=socket;
        let (release,wait)=crossbeam_channel::bounded(1);
        let worker=thread::spawn(move|| {let (control,_)=listener.accept().unwrap();let request:Invocation=receive(&control,TIMEOUT).unwrap();
            assert_eq!(request.method,"raw_source_query");let _=wait.recv_timeout(Duration::from_secs(2));});
        let started=Instant::now();let result=actor.raw_source_query_until(raw_query(),started+Duration::from_millis(40));let elapsed=started.elapsed();
        let _=release.send(());worker.join().unwrap();
        assert!(matches!(result,Err(SourceError::Transport(error)) if error.contains("deadline")));assert!(elapsed<Duration::from_millis(250));
        assert!(actor.pending_inputs().is_empty());
    }
    #[test]
    fn transcript_reads_wait_off_loop_and_old_attachment_results_are_refused() {
        const CHILD:&str="CAGENTS_TRANSCRIPT_READER_CONTROL_TEST";
        if std::env::var_os(CHILD).is_none() {
            let root=tempfile::tempdir_in("/var/tmp").unwrap();
            let test=thread::current().name().unwrap().to_owned();
            let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact",&test,"--nocapture"])
                .env(CHILD,"1").env("XDG_RUNTIME_DIR",root.path()).env("XDG_DATA_HOME",root.path().join("data"))
                .env("XDG_CONFIG_HOME",root.path().join("config")).env("XDG_CACHE_HOME",root.path().join("cache"))
                .env("HOME",root.path().join("home")).env("CAGENTS_LOCAL_DEBUG","0").output().unwrap();
            assert!(output.status.success(),"stdout={} stderr={}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));return;
        }
        for method in ["snapshot","presentation","file_query"] {
            for reattach in [false,true] {
                let mut fixture=super::super::tests::resident_fixture();let server=&mut fixture.server;
                server.proof.boot_id=crate::daemon::read_boot_id().unwrap();server.source.proof=Some(server.proof.clone());
                let paths=crate::daemon::RuntimePaths::discover().unwrap();
                let _lease=crate::daemon::HostLease::acquire(&paths,&server.proof).unwrap();
                let store=server.actor.0.store.clone();let mut held=store.lock().unwrap();
                held.apply(chat::Event::Item(chat::Item {id:"source".into(),role:chat::Role::Assistant,at:None,started_at:None,
                    content:chat::ItemContent::Text {text:"exact retained transcript".into(),thinking:false,phase:chat::Phase::Final}})).unwrap();
                let args=match method {
                    "snapshot"=>json!([null]),
                    "presentation"=>json!([server.source.id,null]),
                    "file_query"=>json!([Command::LoadFileChanges {request_id:42,after:None}]),
                    _=>unreachable!(),
                };
                let request=Invocation {proof:server.proof.clone(),epoch:Some(server.epoch),method:method.into(),args};
                let (client,control)=UnixStream::pair().unwrap();
                assert!(server.dispatch_source_read(control,request).is_ok(),"{method} must use the single bounded reader");
                let started=Instant::now();
                let state=server.invoke(Invocation {proof:server.proof.clone(),epoch:Some(server.epoch),method:"state".into(),args:json!([0])}).unwrap();
                assert!(!state.is_null());assert!(started.elapsed()<Duration::from_secs(1));
                if reattach {
                    let source=server.source.clone();
                    server.invoke(Invocation {proof:server.proof.clone(),epoch:None,method:"attach".into(),args:json!([source])}).unwrap();
                } else {
                    let actor=server.identity.instance;
                    server.invoke(Invocation {proof:server.proof.clone(),epoch:Some(server.epoch),method:"fence_session_stop".into(),args:json!([actor,500])}).unwrap();
                    assert!(server.actor.0.stop.load(Ordering::Acquire),"blocked {method} must not delay explicit stop admission");
                }
                drop(held);
                let response:Result<Value,String>=receive(&client,TIMEOUT).unwrap();
                if reattach {assert!(response.unwrap_err().contains("attachment changed"));}
                else {
                    let response=response.unwrap();
                    match method {
                        "snapshot"=>{let snapshot:Option<Snapshot>=serde_json::from_value(response).unwrap();assert!(snapshot.is_some());},
                        "presentation"=>{let response:Option<crate::terminal_ipc::ServerResponse>=serde_json::from_value(response).unwrap();assert!(response.is_some());},
                        "file_query"=>{let response:Option<chat::FileChangeReply>=serde_json::from_value(response).unwrap();assert!(response.is_some());},
                        _=>unreachable!(),
                    }
                }
            }
        }
    }

    #[test]
    fn transcript_and_output_methods_cannot_fall_back_to_synchronous_control_reads() {
        let mut fixture=super::super::tests::resident_fixture();let server=&mut fixture.server;
        for method in ["snapshot","presentation","file_query","exit_output"] {
            assert!(is_read(method));
            let error=server.invoke(Invocation {proof:server.proof.clone(),epoch:Some(server.epoch),method:method.into(),args:Value::Null}).unwrap_err();
            assert!(error.contains("off-loop reader"),"{method}: {error}");
        }
    }

    #[test]
    fn cache_hydration_holding_resident_state_cannot_block_observation_or_stop_admission() {
        // Snapshot/presentation acquire resident state after Store and may do
        // cold SQLite reads while holding it. Model that later lock stage too,
        // not only the worker waiting to acquire Store in the preceding test.
        let fixture=super::super::tests::resident_fixture();
        let actor=fixture.server.actor.clone();
        let held=actor.0.state.lock().unwrap();
        let (reply,read)=crossbeam_channel::bounded(1);
        let worker=thread::spawn(move|| {
            let mut fixture=fixture;
            let server=&mut fixture.server;
            let observe=server.invoke(Invocation {proof:server.proof.clone(),epoch:Some(server.epoch),method:"observe".into(),args:Value::Null});
            let observation=server.invoke(Invocation {proof:server.proof.clone(),epoch:Some(server.epoch),method:"state".into(),args:json!([0])});
            let actor_id=server.identity.instance;
            let stop=server.invoke(Invocation {proof:server.proof.clone(),epoch:Some(server.epoch),method:"fence_session_stop".into(),args:json!([actor_id,100])});
            let stopping=server.session_stopping;
            let rearmed=server.actor.0.observe_next.load(Ordering::Acquire);
            let _=reply.send((observe,rearmed,observation,stop,stopping));
        });
        let result=read.recv_timeout(Duration::from_secs(1));
        drop(held);
        worker.join().unwrap();
        let (observe,rearmed,observation,stop,stopping)=result.expect("resident disk-read contention must not occupy the helper control loop");
        assert!(observe.is_ok());assert!(rearmed,"the actual proxy observe-before-state ordering must stay nonblocking");
        assert!(observation.unwrap_err().contains("CAGENTS_ACTOR_METADATA_BUSY"));
        assert!(stopping,"the authenticated helper must retain lifetime stop intent before its bounded actor acknowledgement wait");
        if let Err(error)=stop {assert!(error.contains("acknowledgement"),"{error}");}
        let deadline=Instant::now()+Duration::from_secs(1);
        while !actor.0.stop.load(Ordering::Acquire) {
            assert!(Instant::now()<deadline,"the original admitted Stop must complete after cache hydration releases metadata");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn initial_publication_retries_metadata_contention_with_one_bounded_budget() {
        let fixture=super::super::tests::resident_fixture();let actor=fixture.server.actor.clone();
        let held=actor.0.state.lock().unwrap();let (_client,control)=UnixStream::pair().unwrap();
        let (reply,result)=crossbeam_channel::bounded(1);
        let worker=thread::spawn(move|| {
            let publication=fixture.server.startup_attached_until(&control,Instant::now()+Duration::from_secs(1));
            let _=reply.send(publication);
        });
        assert!(matches!(result.recv_timeout(Duration::from_millis(30)),Err(crossbeam_channel::RecvTimeoutError::Timeout)),"temporary actor metadata contention must not terminate startup");
        drop(held);
        let publication=result.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
        assert_eq!(publication.identity.record.holder,Process::read(std::process::id() as i32).unwrap());
        worker.join().unwrap();

        let fixture=super::super::tests::resident_fixture();let actor=fixture.server.actor.clone();
        let held=actor.0.state.lock().unwrap();let (_client,control)=UnixStream::pair().unwrap();
        let started=Instant::now();
        let error=fixture.server.startup_attached_until(&control,started+Duration::from_millis(30)).err().unwrap();
        assert!(error.contains("deadline"));assert!(started.elapsed()<Duration::from_millis(250));
        drop(held);
        assert!(fixture.server.attached().is_ok(),"a metadata timeout does not claim provider death or retire the exact actor");
    }

    #[test]
    fn live_attach_refuses_busy_metadata_before_rotating_epoch_and_retry_keeps_identity() {
        let mut fixture=super::super::tests::resident_fixture();let server=&mut fixture.server;
        let actor=server.actor.clone();let identity=server.identity.clone();let epoch=server.epoch;
        server.attachment_preserved=true;
        let source=server.source.clone();let held=actor.0.state.lock().unwrap();
        let error=server.invoke(Invocation {proof:server.proof.clone(),epoch:None,method:"attach".into(),args:json!([source.clone()])}).unwrap_err();
        assert!(error.contains("CAGENTS_ACTOR_METADATA_BUSY"));assert_eq!(server.epoch,epoch);assert!(server.attachment_preserved);
        drop(held);
        let publication=server.invoke(Invocation {proof:server.proof.clone(),epoch:None,method:"attach".into(),args:json!([source])}).unwrap();
        let publication:Attached=serde_json::from_value(publication).unwrap();
        assert_eq!(publication.identity,identity);assert_ne!(publication.epoch,epoch);assert!(!server.attachment_preserved);
    }

    fn query()->SourceQuery {
        SourceQuery {source:SourceKey {agent_id:Some("child".into()),source_id:"opaque:native-part:real-id".into()},
            offset:4,limit:16,expected_epoch:Some(SourceEpoch {incarnation:uuid::Uuid::new_v4(),revision:7}),until:Some(8)}
    }
    #[test]
    fn held_store_reads_leave_server_control_live_and_reattachment_fences_publication() {
        const CHILD:&str="CAGENTS_SOURCE_READER_CONTROL_TEST";
        if std::env::var_os(CHILD).is_none() {
            let root=tempfile::tempdir_in("/var/tmp").unwrap();
            let test=thread::current().name().unwrap().to_owned();
            let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact",&test,"--nocapture"])
                .env(CHILD,"1").env("XDG_RUNTIME_DIR",root.path()).env("XDG_DATA_HOME",root.path().join("data"))
                .env("XDG_CONFIG_HOME",root.path().join("config")).env("XDG_CACHE_HOME",root.path().join("cache"))
                .env("HOME",root.path().join("home")).env("CAGENTS_LOCAL_DEBUG","0").output().unwrap();
            assert!(output.status.success(),"stdout={} stderr={}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));return;
        }
        for replacement in [false,true] {
            let mut fixture=super::super::tests::resident_fixture();let server=&mut fixture.server;
            server.proof.boot_id=crate::daemon::read_boot_id().unwrap();server.source.proof=Some(server.proof.clone());
            let paths=crate::daemon::RuntimePaths::discover().unwrap();
            let _lease=crate::daemon::HostLease::acquire(&paths,&server.proof).unwrap();
            let store=server.actor.0.store.clone();let mut held=store.lock().unwrap();
            held.apply(chat::Event::Item(chat::Item {id:"source".into(),role:chat::Role::Assistant,at:None,started_at:None,
                content:chat::ItemContent::Text {text:"original cached bytes".into(),thinking:false,phase:chat::Phase::Final}})).unwrap();
            let query=SourceQuery {source:SourceKey {agent_id:None,source_id:"source".into()},offset:0,limit:256,expected_epoch:None,until:None};
            let mut clients=Vec::new();let mut refused=0;
            for _ in 0..QUEUED_READS+2 {
                let (client,control)=UnixStream::pair().unwrap();
                let request=Invocation {proof:server.proof.clone(),epoch:Some(server.epoch),method:"source_query".into(),args:serde_json::to_value(&query).unwrap()};
                match server.dispatch_source_read(control,request) {Ok(())=>clients.push(client),Err((control,_))=>{drop(control);refused+=1;}}
            }
            assert!(!clients.is_empty() && clients.len()<=QUEUED_READS+1 && refused>0,"one worker and fixed queue must bound admission");
            let started=Instant::now();
            let state=server.invoke(Invocation {proof:server.proof.clone(),epoch:Some(server.epoch),method:"state".into(),args:json!([0])}).unwrap();
            assert!(!state.is_null());assert!(started.elapsed()<Duration::from_secs(1));
            if replacement {
                let source=server.source.clone();
                server.invoke(Invocation {proof:server.proof.clone(),epoch:None,method:"attach".into(),args:json!([source])}).unwrap();
            } else {
                let actor=server.identity.instance;
                server.invoke(Invocation {proof:server.proof.clone(),epoch:Some(server.epoch),method:"fence_session_stop".into(),args:json!([actor,500])}).unwrap();
                assert!(server.actor.0.stop.load(Ordering::Acquire),"source IO cannot hold up actual actor stop admission");
            }
            drop(held);
            for client in clients {
                let response:Result<Result<SourcePage,SourceError>,String>=receive(&client,TIMEOUT).unwrap();
                if replacement {assert!(response.unwrap_err().contains("attachment changed"));}
                else {assert_eq!(response.unwrap().unwrap().text,"original cached bytes");}
            }
        }
    }

    #[test]
    fn authenticated_source_read_preserves_store_epoch_and_typed_errors() {
        for changed in [false,true] {
            let root=tempfile::tempdir_in("/var/tmp").unwrap();
            let socket=root.path().join("reader.sock");let listener=UnixListener::bind(&socket).unwrap();
            let mut actor=RemoteActor::fixture();Arc::get_mut(&mut actor.0).unwrap().socket=socket;
            let query=query();let expected=query.clone();let epoch=actor.0.epoch;let proof=actor.0.proof.clone();
            let worker=thread::spawn(move|| {
                let (control,_)=listener.accept().unwrap();let request:Invocation=receive(&control,TIMEOUT).unwrap();
                assert_eq!(request.method,"source_query");assert_eq!(request.epoch,Some(epoch));assert_eq!(request.proof,proof);
                assert_eq!(serde_json::from_value::<SourceQuery>(request.args).unwrap(),expected);
                let result=if changed {Err(SourceError::Changed)} else {Ok(SourcePage {
                    source:expected.source,epoch:expected.expected_epoch.unwrap(),source_len:8,offset:4,next_offset:8,
                    text:"tail".into(),complete:true,metadata:SourceMetadata {role:chat::Role::Assistant,thinking:false,
                        phase:chat::Phase::Final,at:Some(2),started_at:Some(1)},
                })};
                send(&control,&Ok::<_,String>(result),TIMEOUT).unwrap();
            });
            let result=actor.source_query(query.clone());worker.join().unwrap();
            if changed {assert_eq!(result,Err(SourceError::Changed));} else {
                let page=result.unwrap();assert_eq!(Some(page.epoch),query.expected_epoch);assert_eq!(page.text,"tail");
                assert_eq!(page.source,query.source);assert!(page.complete);
            }
            assert!(actor.pending_inputs().is_empty(),"cache reads must not enter the provider command journal");
        }
    }
    #[test]
    fn stalled_authenticated_source_read_uses_one_absolute_budget() {
        let root=tempfile::tempdir_in("/var/tmp").unwrap();let socket=root.path().join("stalled-reader.sock");
        let listener=UnixListener::bind(&socket).unwrap();
        let mut actor=RemoteActor::fixture();Arc::get_mut(&mut actor.0).unwrap().socket=socket;
        let (release,wait)=crossbeam_channel::bounded(1);
        let worker=thread::spawn(move|| {
            let (control,_)=listener.accept().unwrap();let request:Invocation=receive(&control,TIMEOUT).unwrap();
            assert_eq!(request.method,"source_query");let _=wait.recv_timeout(Duration::from_secs(2));
        });
        let started=Instant::now();let result=actor.source_query_until(query(),started+Duration::from_millis(40));
        let elapsed=started.elapsed();let _=release.send(());worker.join().unwrap();
        assert!(matches!(result,Err(SourceError::Transport(error)) if error.contains("deadline")));
        assert!(elapsed<Duration::from_millis(250));assert!(actor.pending_inputs().is_empty());
    }
}
