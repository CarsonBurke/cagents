//! Real helper qualification of the historical Pending -> retired-history join.
//! Only the previous-boot Agent/run are typed fixture data. Installation ACK,
//! provider identity, closed publication and private retirement are real.
use super::*;
use crate::{
    agent_execution::{Destination, HostedSession, Origin, Spec, State, Submit, Surface, Target},
    host::model::Registry,
    process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

fn isolated() -> Option<std::path::PathBuf> {
    const CHILD: &str = "CAGENTS_ARCHIVED_HISTORY_RUNTIME_TEST";
    let test = thread::current().name().unwrap().to_owned();
    if std::env::var(CHILD).as_deref() == Ok(test.as_str()) {
        return Some(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap().into());
    }
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    for child in [
        "home/.claude",
        "run",
        "data",
        "config",
        "cache",
        "bin",
        "project",
        "tmp",
    ] {
        fs::create_dir_all(root.path().join(child)).unwrap();
    }
    let mut command = CommandSpec::new(std::env::current_exe().unwrap());
    // Preserve the caller's opaque delegation and actual matched helper. This
    // child owns only its fresh HOME/runtime/data and never becomes User.
    for (key, _) in std::env::vars_os() {
        if key != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV)
            && key != std::ffi::OsStr::new("CAGENTS_TEST_RETAINED_ACTOR_HELPER")
        {
            command = command.env_remove(key);
        }
    }
    let helper = super::super::retained_actor_tests::retained_helper_executable();
    let command = command
        .args(["--exact", &test, "--nocapture", "--test-threads=1"])
        .cwd(root.path())
        .env(CHILD, &test)
        .env("CAGENTS_TEST_RETAINED_ACTOR_HELPER", helper)
        .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
        .env("CAGENTS_HOLDER_ROOT", root.path())
        .env("HOME", root.path().join("home"))
        .env("CLAUDE_CONFIG_DIR", root.path().join("home/.claude"))
        .env("XDG_RUNTIME_DIR", root.path().join("run"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("TMPDIR", root.path().join("tmp"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.path().join("bin").display()),
        )
        .timeout(Duration::from_secs(90))
        .output_limits(256 * 1024, 256 * 1024);
    let output = SystemProcessExecutor.execute(&command, &CancellationToken::new());
    let retained = root.keep();
    let (stdout, stderr) = match &output {
        Ok(output) => (&output.stdout, &output.stderr),
        Err(error) => (&error.stdout, &error.stderr),
    };
    fs::write(retained.join("test.stdout"), stdout).unwrap();
    fs::write(retained.join("test.stderr"), stderr).unwrap();
    let output = output.expect("bounded archived-history child must settle");
    assert!(
        output.success,
        "archived-history fixture {}:\n{}\n{}",
        retained.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("archived-history fixture evidence: {}", retained.display());
    None
}

fn wait(mut predicate: impl FnMut() -> bool, reason: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "{reason}");
        let ready = predicate();
        assert!(Instant::now() < deadline, "{reason}");
        if ready {
            return;
        }
        thread::sleep(
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

macro_rules! inspect {
    ($fixture:expr) => {{
        let proof = $fixture.host.lock().unwrap().proof().clone();
        Handle::inspect_remote(&$fixture.identity.record, proof).unwrap()
    }};
}

struct Fixture {
    root: std::path::PathBuf,
    host: SharedHost,
    _lease: crate::daemon::HostLease,
    handle: Handle,
    identity: chat::runtime::ActorIdentity,
    source: Session,
    historical: crate::agent_execution::Agent,
    run: crate::agent_execution::Run,
    _context: Arc<chat::protocol::ExecutionContext>,
}

impl Fixture {
    fn new(root: std::path::PathBuf) -> Self {
        install_provider(&root.join("bin/claude"));
        let proof = HostSessionProof {
            host_session_id: "archived-history-runtime".into(),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: "actual-current-owner".into(),
        };
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        let mut state = HostState::new(proof.clone());
        state.register_client(17);
        state.install_model(Registry::at(&root.join("registry.sqlite3")).unwrap());
        let host = Arc::new(HostLock::new(state));
        let mut source = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            root.join("project"),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
        source.proof = Some(proof.clone());
        assert_eq!(
            source.native_continuity,
            chat::NativeContinuity::ReconnectOnly
        );
        let mut historical = source.clone();
        historical.proof.as_mut().unwrap().boot_id = "synthetic-previous-boot-fixture".into();
        let registry = super::super::super::agent_execution::registry(&host).unwrap();
        let (agent, run) = {
            let mut store = registry.lock().unwrap();
            store
                .apply(&crate::workspace_model::ModelEdit::OpenProject {
                    repository: root.join("project"),
                    name: "Archived history runtime".into(),
                    kind: crate::model::ProjectKind::Folder,
                })
                .unwrap();
            let accepted = store
                .submit_agent(
                    &Origin::User,
                    &Submit {
                        name: None,
                        destination: Destination::New(Spec {
                            provider: Provider::Claude,
                            account: None,
                            target: Target {
                                repository: root.join("project"),
                                directory: root.join("project"),
                                placement: None,
                            },
                            surface: Surface::Chat,
                            model: None,
                            permission_mode: None,
                            reasoning_effort: None,
                        }),
                        prompt: "Historical fixture work; never dispatch this request".into(),
                        idempotency_key: "historical-runtime-join".into(),
                    },
                    1,
                )
                .unwrap();
            store
                .bind_agent(
                    accepted.agent.id,
                    None,
                    HostedSession::Chat(historical.clone()),
                )
                .unwrap();
            store
                .fresh_agent_authority(&HostedSession::Chat(historical))
                .unwrap();
            let started = store
                .update_agent_run(
                    accepted.run.id,
                    accepted.run.revision,
                    crate::agent_execution::Update::Started {
                        provider_key: "synthetic-previous-boot-result".into(),
                    },
                    2,
                )
                .unwrap();
            let run = store
                .update_agent_run(
                    started.id,
                    started.revision,
                    crate::agent_execution::Update::Finished {
                        state: State::Succeeded,
                        result: crate::agent_execution::ResultSummary {
                            text: "Preserve historical result".into(),
                            ..Default::default()
                        },
                    },
                    3,
                )
                .unwrap();
            (store.agent(accepted.agent.id).unwrap(), run)
        };
        let context =
            Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap());
        let (mut spec, adapter) = chat::protocol::create(&source.launch).unwrap();
        spec.execution = Some(context.clone());
        let prepared = adapter.prepared_launch_identity().unwrap();
        let environment = spec.environment.clone();
        let handle =
            Handle::start_remote_owned(spec, source.clone(), prepared, environment, |_| {})
                .unwrap();
        let identity = handle.actor_identity().unwrap();
        wait(
            || handle.attachment_snapshot().ready && handle.confirmed_session_id().is_some(),
            "real provider did not confirm its initialized conversation",
        );
        source.launch.session_id = handle.confirmed_session_id();
        // The old immutable canonical row remains unnamed. The helper must
        // independently issue this confirmed ReconnectOnly cleanup descriptor
        // before enrollment; no guessed UUID or alias is admitted.
        Handle::inspect_remote(&identity.record, proof.clone()).unwrap();
        handle
            .update_native_source(source.clone(), None, None)
            .unwrap();
        let inspection = Handle::inspect_remote(&identity.record, proof.clone()).unwrap();
        assert!(!inspection.state.installed);
        enroll_pending(
            &host,
            &identity,
            &inspection.source,
            &inspection.previous_sources,
            inspection
                .launch_account_scope
                .as_ref()
                .map_err(String::as_str),
            false,
        )
        .unwrap();
        handle.commit_native_installation().unwrap();
        context.installed();
        let installed = Handle::inspect_remote(&identity.record, proof).unwrap();
        assert!(installed.state.installed);
        assert_eq!(installed.state.confirmed_session, source.launch.session_id);
        source.native_continuity = installed.state.native_continuity;
        handle
            .update_native_source(source.clone(), None, None)
            .unwrap();
        // Stop at the real ACK-before-canonical-publication cutpoint. Do not
        // promote Pending or rewrite the synthetic old canonical Agent.
        let ownership = registry
            .lock()
            .unwrap()
            .native_actor_ownership(identity.instance)
            .unwrap()
            .unwrap();
        assert!(ownership.is_pending());
        assert_eq!(
            ownership.source().native_continuity,
            chat::NativeContinuity::ReconnectOnly
        );
        assert_eq!(registry.lock().unwrap().agent(agent.id).unwrap(), agent);
        Self {
            root,
            host,
            _lease: lease,
            handle,
            identity,
            source,
            historical: agent,
            run,
            _context: context,
        }
    }

    fn registry(&self) -> super::super::super::agent_execution::SharedRegistry {
        super::super::super::agent_execution::registry(&self.host).unwrap()
    }

    fn assert_preserved(&self) {
        let store = self.registry();
        let store = store.lock().unwrap();
        assert_eq!(store.agent(self.historical.id).unwrap(), self.historical);
        assert_eq!(store.agent_run(self.run.id).unwrap(), self.run);
        assert!(
            store
                .native_actor_retirement(self.identity.instance)
                .unwrap()
                .is_none()
        );
        assert!(self.host.lock().unwrap().chats.is_empty());
        assert!(
            !self.root.join("prompts").exists(),
            "recovery injected provider input"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let handles: Vec<_> = self
            .host
            .lock()
            .unwrap()
            .chats
            .values()
            .map(|record| record.handle.clone())
            .collect();
        for handle in handles {
            let _ = handle.close();
            let _ = handle.finish_retained_close();
            if let Some(identity) = handle.actor_identity() {
                for process in [&identity.record.root, &identity.record.holder] {
                    if process.is_current() {
                        unsafe {
                            libc::kill(process.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
        // Failure containment only; this is never the successful close oracle.
        for process in [&self.identity.record.root, &self.identity.record.holder] {
            if process.is_current() {
                unsafe {
                    libc::kill(process.pid, libc::SIGKILL);
                }
            }
        }
    }
}

#[test]
fn authenticated_legacy_archive_joins_retirement_and_prepares_explicit_resume_without_input() {
    let Some(root) = isolated() else { return };
    let fixture = Fixture::new(root);
    fixture.handle.close().unwrap();
    let closed = inspect!(fixture);
    assert!(closed.state.installed && closed.state.close.completed && closed.state.close.exited);
    assert_eq!(closed.state.outbox_ack, closed.state.outbox_latest);
    assert!(closed.state.continuity_loss.is_none());
    assert_eq!(closed.source, fixture.source);
    assert_eq!(
        closed.source.native_continuity,
        chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
    );
    reconcile_previous_boot_history(&fixture.host).unwrap();
    let store = fixture.registry();
    let (ownership, retirement) = {
        let store = store.lock().unwrap();
        let mut archived = fixture.historical.clone();
        archived.closed = true;
        assert_eq!(store.agent(archived.id).unwrap(), archived);
        assert_eq!(store.agent_run(fixture.run.id).unwrap(), fixture.run);
        let ownership = store
            .native_actor_ownership(fixture.identity.instance)
            .unwrap()
            .unwrap();
        let retirement = store
            .native_actor_retirement(fixture.identity.instance)
            .unwrap()
            .unwrap();
        assert!(ownership.is_pending() && retirement.is_pending());
        assert!(
            store
                .archived_history(
                    &fixture.source,
                    &ownership,
                    &retirement,
                    &fixture.host.lock().unwrap().proof().boot_id
                )
                .unwrap()
                .is_some()
        );
        (ownership, retirement)
    };
    let proof = fixture.host.lock().unwrap().proof().clone();
    let RetirementInspection::Verified(verified) =
        Handle::inspect_retained_retirement(&fixture.identity.record, &proof).unwrap()
    else {
        panic!("normal reconciliation produced no authenticated private retirement");
    };
    assert!(verified.receipt().installed);
    assert_eq!(verified.receipt().source, fixture.source);
    assert!(!fixture.identity.record.root.is_current());
    // This executes validate_evidence(..., Some(ArchivedHistory)) with the
    // actual VerifiedRetirement, physical account and current daemon lease.
    assert!(
        super::super::retained_resume::prepare_resume_verified(
            &fixture.host,
            &fixture.source,
            Some(verified)
        )
        .unwrap()
    );
    let (_, mut history) = chat::protocol::create_history(&fixture.source.launch).unwrap();
    let loaded = history.initialize().unwrap();
    assert!(
        loaded
            .events
            .iter()
            .any(|event| matches!(event, chat::Event::History { items, .. } if !items.is_empty()))
    );
    let prepared = super::super::process_inner(
        &fixture.host,
        17,
        ClientRequest::ChatPrepareResume {
            source: fixture.source.clone(),
        },
    )
    .unwrap();
    let ServerResponse::ChatResumePrepared { session } = prepared else {
        panic!("explicit resume was not prepared")
    };
    assert_ne!(session.id, fixture.source.id);
    assert_eq!(session.launch, fixture.source.launch);
    assert_eq!(session.proof, None);
    assert_eq!(
        session.native_continuity,
        chat::NativeContinuity::ReconnectOnly
    );
    assert!(fixture.host.lock().unwrap().chats.is_empty());
    assert_eq!(
        fs::read_to_string(fixture.root.join("launches"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(!fixture.root.join("prompts").exists());
    assert_eq!(
        store
            .lock()
            .unwrap()
            .native_actor_ownership(fixture.identity.instance)
            .unwrap(),
        Some(ownership)
    );
    assert_eq!(
        store
            .lock()
            .unwrap()
            .native_actor_retirement(fixture.identity.instance)
            .unwrap(),
        Some(retirement)
    );
    // Only a separate, explicit Start may create the new writer. It must use
    // the same confirmed conversation and must not replay the old run/prompt.
    let resumed_id = session.id;
    let started = super::super::process_inner(
        &fixture.host,
        17,
        ClientRequest::ChatStart {
            session,
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    )
    .unwrap();
    assert!(matches!(started, ServerResponse::ChatAttached { .. }));
    let resumed = fixture.host.lock().unwrap().chats[&resumed_id]
        .handle
        .clone();
    wait(
        || {
            resumed.attachment_snapshot().ready
                && resumed.confirmed_session_id() == fixture.source.launch.session_id
        },
        "explicit writer did not confirm the exact archived conversation",
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("launches"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert!(
        !fixture.root.join("prompts").exists(),
        "explicit resume replayed a historical prompt"
    );
    super::super::process_inner(
        &fixture.host,
        17,
        ClientRequest::ChatClose {
            chat_id: resumed_id,
        },
    )
    .unwrap();
}

#[test]
fn authenticated_legacy_archive_preserves_active_and_unacknowledged_helpers() {
    let Some(root) = isolated() else { return };
    let fixture = Fixture::new(root);
    fs::write(fixture.root.join("emit-permission"), "emit").unwrap();
    wait(
        || {
            let state = inspect!(fixture).state;
            state.owns_work && state.outbox_latest > state.outbox_ack
        },
        "real permission request did not retain work and an unacknowledged outcome",
    );
    reconcile_previous_boot_history(&fixture.host).unwrap();
    fixture.assert_preserved();
    fixture.handle.close().unwrap();
    let closed = inspect!(fixture);
    assert!(closed.state.close.completed && closed.state.close.exited);
    assert!(closed.state.outbox_latest > closed.state.outbox_ack);
    assert!(
        fixture.handle.finish_retained_close().is_err(),
        "unacknowledged helper retired"
    );
    reconcile_previous_boot_history(&fixture.host).unwrap();
    fixture.assert_preserved();
}

#[test]
fn authenticated_legacy_archive_preserves_an_unproved_helper_exit() {
    let Some(root) = isolated() else { return };
    let fixture = Fixture::new(root);
    assert!(fixture.identity.record.holder.is_current());
    assert_eq!(
        unsafe { libc::kill(fixture.identity.record.holder.pid, libc::SIGKILL) },
        0
    );
    let proof = fixture.host.lock().unwrap().proof().clone();
    wait(
        || {
            matches!(
                Handle::inspect_retained_retirement(&fixture.identity.record, &proof),
                Ok(RetirementInspection::ExitedUnproved)
            )
        },
        "killed helper did not expose its unproved retirement",
    );
    reconcile_previous_boot_history(&fixture.host).unwrap();
    fixture.assert_preserved();
}

#[test]
fn authenticated_legacy_archive_preserves_actual_helper_continuity_loss() {
    let Some(root) = isolated() else { return };
    let fixture = Fixture::new(root);
    // The ordinary provider protocol generates enough distinct waiting events
    // to exceed the production journal entry bound. No journal/DB is edited.
    fs::write(fixture.root.join("emit-loss"), "emit").unwrap();
    wait(
        || inspect!(fixture).state.continuity_loss.is_some(),
        "real provider events did not produce authenticated journal overflow",
    );
    fixture.handle.close().unwrap();
    let closed = inspect!(fixture);
    assert!(closed.state.close.completed && closed.state.close.exited);
    assert!(closed.state.continuity_loss.is_some());
    assert!(fixture.handle.finish_retained_close().is_err());
    reconcile_previous_boot_history(&fixture.host).unwrap();
    fixture.assert_preserved();
}

fn install_provider(path: &Path) {
    fs::write(path, r#"#!/usr/bin/python3
import json, os, pathlib, select, sys
root=pathlib.Path(os.environ['HOME']).parent
with (root/'launches').open('a') as log: log.write('launch\n')
args=sys.argv[1:]
session=next((a.split('=',1)[1] for a in args if a.startswith('--session-id=')), None)
if session is None:
    session=args[args.index('--resume')+1]
initialized=False
sent=False
loss_sent=False
def send(value): print(json.dumps(value),flush=True)
while True:
    if initialized and not loss_sent and (root/'emit-loss').exists():
        for index in range(4100):
            request='archive-overflow-'+str(index)
            send({'type':'control_request','request_id':request,'request':{'subtype':'can_use_tool','tool_name':'Read','tool_use_id':request,'input':{'file_path':'/fixture/read-only'}}})
            send({'type':'control_cancel_request','request_id':request})
        loss_sent=True
    if initialized and not sent and (root/'emit-permission').exists():
        send({'type':'control_request','request_id':'archive-permission','request':{'subtype':'can_use_tool','tool_name':'Read','tool_use_id':'archive-read','input':{'file_path':'/fixture/read-only'}}})
        sent=True
    ready,_,_=select.select([sys.stdin],[],[],0.02)
    if not ready: continue
    line=sys.stdin.readline()
    if not line: break
    frame=json.loads(line)
    if frame.get('type')=='control_request':
        send({'type':'control_response','response':{'subtype':'success','request_id':frame['request_id'],'response':{}}})
        if frame.get('request',{}).get('subtype')=='initialize':
            project=''.join(c if c.isascii() and c.isalnum() else '-' for c in os.getcwd())
            directory=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'])/'projects'/project
            directory.mkdir(parents=True,exist_ok=True)
            records=[{'type':'user','uuid':'archive-user','sessionId':session,'message':{'role':'user','content':'Saved fixture question'}},
                     {'type':'assistant','uuid':'archive-assistant','parentUuid':'archive-user','sessionId':session,'message':{'id':'archive-message','role':'assistant','content':[{'type':'text','text':'Saved fixture answer'}]}}]
            (directory/(session+'.jsonl')).write_text(''.join(json.dumps(v)+'\n' for v in records))
            send({'type':'system','subtype':'init','session_id':session})
            initialized=True
    elif frame.get('type')=='user':
        with (root/'prompts').open('a') as log: log.write('prompt\n')
"#).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
