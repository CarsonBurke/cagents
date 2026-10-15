//! Typed original-source reads use the actual ingestion Store in the helper.
//! Dependency: source transport/Store foundation through cli_contracts ee6993844.
use super::*;
use crate::chat::cache::{SourceError, SourcePage, SourceQuery};

impl RemoteActor {
    pub fn source_query(&self, query:SourceQuery)->Result<SourcePage,SourceError> {
        self.source_query_until(query,Instant::now()+TIMEOUT)
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
    fn query()->SourceQuery {
        SourceQuery {source:SourceKey {agent_id:Some("child".into()),source_id:"opaque:native-part:real-id".into()},
            offset:4,limit:16,expected_epoch:Some(SourceEpoch {incarnation:uuid::Uuid::new_v4(),revision:7}),until:Some(8)}
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
