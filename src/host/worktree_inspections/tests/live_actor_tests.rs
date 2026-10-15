//! Actual request/census/fence/cleanup contracts with quiet local /bin/cat
//! actors. These are lifecycle fixtures, not installed provider qualification.
use super::*;
use crate::{chat, model::TerminalId, worktree_control::SessionIdentity};
use std::{collections::BTreeMap, fs, os::fd::{AsRawFd,FromRawFd}, sync::mpsc, thread};

struct Actor {
    id: TerminalId,
    handle: chat::runtime::Handle,
    process: crate::daemon::holder_registry::Process,
    pin: fs::File,
}
impl Actor {
    fn start(host: &SharedHost, root: &Path) -> Self {
        struct Quiet;
        impl chat::protocol::Adapter for Quiet {
            fn initialize(&mut self) -> Result<chat::protocol::Step, String> { Ok(Default::default()) }
            fn command(&mut self, _: chat::Command) -> Result<chat::protocol::Step, String> { Ok(Default::default()) }
            fn receive(&mut self, _: serde_json::Value) -> Result<chat::protocol::Step, String> { Ok(Default::default()) }
        }
        let id = TerminalId::new();
        let pid_file = root.join(".git").join(format!("fixture-pid-{id}"));
        // Linked worktrees have a .git file, so use the private fixture parent
        // when the administrative marker cannot contain a file.
        let pid_file = if root.join(".git").is_dir() { pid_file } else {
            root.parent().unwrap().join(format!("fixture-pid-{id}"))
        };
        let recipe = crate::model::LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "printf %s $$ > \"$1\"; exec /bin/cat".into(), "fixture".into(), pid_file.display().to_string()],
            working_directory: root.into(), environment: BTreeMap::new(), unset_environment: Default::default(),
        };
        let context = Arc::new(chat::protocol::ExecutionContext::new(Some(Arc::new(fs::File::open(root).unwrap()))));
        context.observe_provider_directory(root).unwrap();
        let spec = chat::protocol::ProcessSpec {
            execution: Some(context), messages: None, unset_environment: Default::default(),
            transport: chat::protocol::TransportSpec::JsonLines,
            program: recipe.program.clone(), arguments: recipe.arguments.clone(),
            working_directory: root.into(), environment: BTreeMap::new(),
        };
        let mut initial = chat::protocol::Step { events: vec![chat::Event::Ready(true),chat::Event::TurnStarted], ..Default::default() };
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let handle = chat::runtime::Handle::start(spec,Box::new(Quiet),initial,store,None,|_|{}).unwrap();
        let deadline = Instant::now()+Duration::from_secs(3);
        let process = loop {
            if let Ok(pid) = fs::read_to_string(&pid_file).and_then(|s| s.parse::<i32>().map_err(std::io::Error::other)) {
                let process = crate::daemon::holder_registry::Process::read(pid).unwrap();
                if handle.close_observation().ready && fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| exe.file_name().is_some_and(|name|name=="cat")) { break process; }
            }
            assert!(Instant::now()<deadline,"actual local provider did not become ready");
            thread::sleep(Duration::from_millis(2));
        };
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, process.pid, 0) };
        assert!(descriptor >= 0,"capture the original provider liveness observation");
        let pin = unsafe { fs::File::from_raw_fd(descriptor as i32) };
        assert_eq!(crate::daemon::holder_registry::Process::read(process.pid).unwrap(),process);
        let actor = Self {id,handle:handle.clone(),process,pin};
        host.lock().unwrap().chats.insert(id,super::super::super::chat::Record {
            execution_instance:uuid::Uuid::new_v4(),console_recipe:None,paired_terminal:None,
            session:chat::Session {id,proof:None,terminal:None,owned_codex_listener:None,native_continuity:Default::default(),
                launch:chat::Launch {provider:crate::agent::Provider::Claude,session_id:None,recipe}},
            handle,source_process:None,subscribers:BTreeMap::new(),observations:BTreeMap::new(),completion_observed:false,returning_terminal:false,
        });
        actor
    }
    fn assert_live(&self, host: &SharedHost) {
        assert_eq!(crate::daemon::holder_registry::Process::read(self.process.pid).unwrap(),self.process);
        let mut descriptor=libc::pollfd {fd:self.pin.as_raw_fd(),events:libc::POLLIN,revents:0};
        assert_eq!(unsafe {libc::poll(&mut descriptor,1,0)},0,"same PID/start alone must not classify an unreaped zombie as live");
        assert!(!self.handle.is_completed());
        let state=host.lock().unwrap();
        assert!(state.chats.get(&self.id).is_some_and(|record|record.handle.same_actor(&self.handle)));
    }
    fn assert_reaped(&self) {
        assert!(self.handle.is_completed());
        let mut descriptor=libc::pollfd {fd:self.pin.as_raw_fd(),events:libc::POLLIN,revents:0};
        assert_eq!(unsafe {libc::poll(&mut descriptor,1,0)},1);
        assert_ne!(descriptor.revents & libc::POLLIN,0,"original pinned provider exited");
        match crate::daemon::holder_registry::Process::read(self.process.pid) {
            Ok(current)=>assert_ne!(current,self.process,"original provider must have been reaped"),
            Err(error)=>assert!(error.kind()==std::io::ErrorKind::NotFound || error.raw_os_error()==Some(libc::ESRCH),"unexpected process observation: {error}"),
        }
    }
}
impl Drop for Actor {
    fn drop(&mut self) {
        // The runtime retains the exact original Child. Never signal a numeric
        // PID from the observation above or discard an uncertain owner.
        if let Err(error)=self.handle.close_until(Instant::now()+Duration::from_secs(3)) {
            eprintln!("private actor {} retained after cleanup uncertainty: {error}",self.id);
        }
    }
}
fn committed(root: &Path) {
    git(root,&["commit","--quiet","--allow-empty","-m","Fixture"]);
}
fn consent(host:&SharedHost,repository:&Path,path:&Path)->SessionStopConsent {
    match request(host,1,Operation::BusySessions {repository:repository.into(),path:path.into()}).unwrap() {
        Reply::BusySessions{census,..}=>SessionStopConsent{sessions:census.into_iter().map(|actor|SessionIdentity{session:actor.session,instance:actor.session_instance}).collect()},
        _=>panic!("actual census required"),
    }
}
fn stop_with(repository:&Path,path:&Path,fingerprint:String,token:uuid::Uuid,consent:SessionStopConsent)->Operation {
    Operation::StopSessions {repository:repository.into(),path:path.into(),consent,expected_fingerprint:fingerprint,stop_authority:token}
}
fn safety(root:&Path)->crate::repository::WorktreeSafetySnapshot { RepositoryService::system("git").worktree_safety(root,&CancellationToken::new()).unwrap() }
fn fingerprint(root:&Path)->String { safety(root).fingerprint }
fn cloned(parent:&Path,root:&Path)->PathBuf {
    let clone=parent.join("replacement");
    git(parent,&["clone","--quiet","--no-hardlinks",root.to_str().unwrap(),clone.to_str().unwrap()]);
    git(&clone,&["remote","remove","origin"]);
    let original=safety(root);let replacement=safety(&clone);
    assert_eq!((original.modified,original.untracked),(0,0));
    assert_eq!((replacement.modified,replacement.untracked),(0,0));
    assert!(original.head_oid.is_some());
    assert_eq!(original.head_oid,replacement.head_oid,"actual same commit");
    assert_eq!(original.branch,replacement.branch,"actual same branch");
    assert_eq!(original.fingerprint,replacement.fingerprint,"clean clone has same safety projection");
    clone
}
fn inspect_at(host:&SharedHost,repository:&Path,path:&Path)->(String,uuid::Uuid) {
    match request(host,1,Operation::InspectForSessionStop{repository:repository.into(),path:path.into()}).unwrap() {
        Reply::Inspected{snapshot,stop_authority:Some(token),..}=>(serde_json::from_str::<crate::repository::WorktreeSafetySnapshot>(&snapshot).unwrap().fingerprint,token),
        _=>panic!("original inspection required"),
    }
}

#[test]
fn original_inspection_stops_only_reviewed_live_actor_and_cannot_replay() {
    const TEST:&str="host::worktree_inspections::tests::live_actor_tests::original_inspection_stops_only_reviewed_live_actor_and_cannot_replay";
    if super::private_runtime::enter(TEST).is_none(){return;}
    let (parent,root)=repository();committed(&root);let other=parent.path().join("other");
    git(&root,&["worktree","add","--quiet","-b","other",other.to_str().unwrap()]);
    let host=host();let (fp,token)=inspect_target(&host,&root);
    // The physical inspection is retained while explicit session consent is
    // freshly reviewed. No second Inspect may recapture pathname authority.
    let selected=Actor::start(&host,&root);let unrelated=Actor::start(&host,&other);
    let roster=consent(&host,&root,&root);assert_eq!(roster.sessions.len(),1);assert_eq!(roster.sessions[0].session,selected.id);
    assert!(matches!(request(&host,1,stop_with(&root,&root,fp.clone(),token,roster.clone())).unwrap(),Reply::SessionsStopped{..}));
    selected.assert_reaped();unrelated.assert_live(&host);
    assert!(!host.lock().unwrap().chats.contains_key(&selected.id));
    assert!(request(&host,1,stop_with(&root,&root,fp,token,roster)).is_err());
    unrelated.assert_live(&host);
}

#[test]
fn clean_same_path_same_head_clone_preserves_original_live_actor() {
    const TEST:&str="host::worktree_inspections::tests::live_actor_tests::clean_same_path_same_head_clone_preserves_original_live_actor";
    if super::private_runtime::enter(TEST).is_none(){return;}
    let (parent,root)=repository();committed(&root);let replacement=cloned(parent.path(),&root);let host=host();
    let actor=Actor::start(&host,&root);let (fp,token)=inspect_target(&host,&root);let roster=consent(&host,&root,&root);
    fs::rename(&root,parent.path().join("original")).unwrap();fs::rename(replacement,&root).unwrap();
    assert_eq!(fingerprint(&root),fp);
    assert!(request(&host,1,stop_with(&root,&root,fp,token,roster)).is_err());
    actor.assert_live(&host);
    // Attribution can refuse before the final pin check when the original
    // admitted directory moved. This still tests actual caller no-teardown.
}

#[test]
fn final_off_host_validation_after_census_refuses_replaced_admin_before_first_fence() {
    const TEST:&str="host::worktree_inspections::tests::live_actor_tests::final_off_host_validation_after_census_refuses_replaced_admin_before_first_fence";
    if super::private_runtime::enter(TEST).is_none(){return;}
    let (parent,root)=repository();committed(&root);let replacement=cloned(parent.path(),&root);let host=host();let actor=Actor::start(&host,&root);
    let (fp,token)=inspect_target(&host,&root);let roster=consent(&host,&root,&root);
    thread::scope(|scope| {
        let (entered_tx,entered_rx)=mpsc::sync_channel(1);let (release_tx,release_rx)=mpsc::sync_channel(1);
        let host_for_stop=host.clone();let root_for_stop=root.clone();let fp_for_stop=fp.clone();
        let (done_tx,done_rx)=mpsc::sync_channel(1);
        scope.spawn(move||{
            super::super::VALIDATION_HOOK.with(|slot|*slot.borrow_mut()=Some(Box::new(move||{
                entered_tx.send(()).unwrap();release_rx.recv_timeout(Duration::from_secs(5)).expect("finite validation barrier");
            })));
            struct Clear;
            impl Drop for Clear {fn drop(&mut self){super::super::VALIDATION_HOOK.with(|slot|{slot.borrow_mut().take();});}}
            let _clear=Clear;
            let result=request(&host_for_stop,1,stop_with(&root_for_stop,&root_for_stop,fp_for_stop,token,roster));
            done_tx.send(result).unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // A second real HostLock acquisition must complete while validation is
        // paused; it neither acquires operation admission nor performs IO.
        let (probe_tx,probe_rx)=mpsc::sync_channel(1);let probe=host.clone();
        scope.spawn(move||{let state=probe.lock().unwrap();probe_tx.send(state.chats.len()).unwrap();});
        assert_eq!(probe_rx.recv_timeout(Duration::from_secs(1)).unwrap(),1);
        fs::rename(root.join(".git"),parent.path().join("original-admin")).unwrap();
        fs::rename(replacement.join(".git"),root.join(".git")).unwrap();
        assert_eq!(fingerprint(&root),fp,"the last fence must use retained physical identity, not equality of the reread projection");
        release_tx.send(()).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap().is_err());
    });
    actor.assert_live(&host);
    assert!(!actor.handle.close_observation().closing,"no first stop fence was admitted");
}

#[test]
fn changed_symlink_target_cannot_spend_original_inspection_with_fresh_other_actor_consent() {
    const TEST:&str="host::worktree_inspections::tests::live_actor_tests::changed_symlink_target_cannot_spend_original_inspection_with_fresh_other_actor_consent";
    if super::private_runtime::enter(TEST).is_none(){return;}
    let (parent,root)=repository();committed(&root);let other=parent.path().join("other");
    git(&root,&["worktree","add","--quiet","-b","other",other.to_str().unwrap()]);
    let alias=parent.path().join("selected");std::os::unix::fs::symlink(&root,&alias).unwrap();
    let host=host();let a=Actor::start(&host,&root);let b=Actor::start(&host,&other);
    let (fp,token)=inspect_at(&host,&root,&alias);
    let (same_fp,original_roster_token)=inspect_at(&host,&root,&alias);
    assert_eq!(same_fp,fp);let original_a=consent(&host,&root,&alias);
    fs::remove_file(&alias).unwrap();std::os::unix::fs::symlink(&other,&alias).unwrap();
    let fresh_b=consent(&host,&root,&alias);assert_eq!(fresh_b.sessions[0].session,b.id);
    assert!(request(&host,1,stop_with(&root,&alias,fp.clone(),token,fresh_b)).is_err());
    // Matching A consent passes the captured canonical census; the original
    // lexical-alias check itself must still refuse the redirected request.
    assert!(request(&host,1,stop_with(&root,&alias,fp,original_roster_token,original_a)).is_err());
    a.assert_live(&host);b.assert_live(&host);
    assert!(!a.handle.close_observation().closing && !b.handle.close_observation().closing);
}

#[test]
fn disconnected_original_capability_preserves_actor_and_fresh_explicit_review_can_stop() {
    const TEST:&str="host::worktree_inspections::tests::live_actor_tests::disconnected_original_capability_preserves_actor_and_fresh_explicit_review_can_stop";
    if super::private_runtime::enter(TEST).is_none(){return;}
    let (_parent,root)=repository();committed(&root);let host=host();let actor=Actor::start(&host,&root);
    let (fp,token)=inspect_target(&host,&root);let roster=consent(&host,&root,&root);
    host.lock().unwrap().unregister_client(1);host.lock().unwrap().register_client(1);
    assert!(request(&host,1,stop_with(&root,&root,fp,token,roster)).is_err());actor.assert_live(&host);
    let (fp,token)=inspect_target(&host,&root);let fresh=consent(&host,&root,&root);
    assert!(matches!(request(&host,1,stop_with(&root,&root,fp,token,fresh)).unwrap(),Reply::SessionsStopped{..}));actor.assert_reaped();
}

#[test]
fn physical_change_after_fence_refuses_cleanup_and_retains_exact_owner() {
    const TEST:&str="host::worktree_inspections::tests::live_actor_tests::physical_change_after_fence_refuses_cleanup_and_retains_exact_owner";
    if super::private_runtime::enter(TEST).is_none(){return;}
    let (parent,root)=repository();committed(&root);let replacement=cloned(parent.path(),&root);let host=host();let actor=Actor::start(&host,&root);
    let (fp,token)=inspect_target(&host,&root);let roster=consent(&host,&root,&root);
    let gate=host.lock().unwrap().control_gate.clone();
    let mut pending={let _admission=gate.write().unwrap();super::super::super::worktree_sessions::prepare_inspected_stop(&host,1,&root,&root,&roster,&fp,token).unwrap()};
    fs::rename(root.join(".git"),parent.path().join("original-admin")).unwrap();fs::rename(replacement.join(".git"),root.join(".git")).unwrap();
    assert_eq!(fingerprint(&root),fp);
    assert!(pending.cleanup().is_err());
    assert!(host.lock().unwrap().chats.get(&actor.id).is_some_and(|record|record.handle.same_actor(&actor.handle)), "cleanup refusal retains the exact record");
    // Stop admission already fenced this actor and its runtime may already
    // be closing/reaping the provider. No continued-process claim is made.
    // Preserve that truthful state;
    // a failed cleanup is not a rollback of user consent or positive cleanup.
    assert!(actor.handle.close_observation().closing);
    drop(pending);
}
