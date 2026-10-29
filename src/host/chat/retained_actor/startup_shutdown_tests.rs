//! Exact admission unit regressions plus one real helper semantic retry. The
//! in-memory proxy fixtures below never exercise helper authentication; the
//! final isolated test uses the production constructor and close protocol.
use super::*;
use crate::chat::protocol::{LaunchCleanup, LaunchOutcome};

struct Pending;
impl LaunchCleanup for Pending {
    fn cleanup(&self) -> Result<LaunchOutcome, String> {
        Ok(LaunchOutcome::Uncertain)
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    host: SharedHost,
    pending: Startup,
    identity: chat::runtime::ActorIdentity,
    attachment: Handle,
    successor: Handle,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let (attachment, successor) = Handle::fixture_remote_attachment_pair();
        let identity = attachment.actor_identity().unwrap();
        let proof = HostSessionProof {
            host_session_id: "semantic-startup-capture".into(),
            boot_id: identity.record.boot.clone(),
            host_incarnation: "original".into(),
        };
        let source = Session {
            id: identity.record.terminal,
            launch: chat::Launch {
                provider: Provider::Claude,
                session_id: None,
                recipe: crate::model::LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: root.path().into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::ReconnectOnly,
        };
        let context = Arc::new(chat::protocol::ExecutionContext::default());
        let token = context.claim_startup_owner(Arc::new(Pending));
        assert!(context.transfer_startup_owner(token, Arc::new(Pending), identity.clone()));
        let pending = Startup {
            source: source.clone(),
            requested: source,
            context,
            failed_close: None,
        };
        let mut state = HostState::new(proof);
        state.install_model(
            crate::host::model::Registry::at(&root.path().join("registry.sqlite3")).unwrap(),
        );
        state
            .native_actor_startups
            .insert(pending.source.id, pending.clone());
        state.shutting_down = true;
        let host = Arc::new(HostLock::new(state));
        pending.context.request_ordinary_cleanup();
        Self {
            _root: root,
            host,
            pending,
            identity,
            attachment,
            successor,
        }
    }

    fn capture(&self) -> Capture {
        capture_startup(&self.host.lock().unwrap(), &self.pending).unwrap()
    }

    fn record(&self, handle: Handle) -> Record {
        Record {
            execution_instance: self.identity.instance,
            console_recipe: None,
            paired_terminal: None,
            session: self.pending.source.clone(),
            handle,
            source_process: None,
            subscribers: BTreeMap::new(),
            observations: BTreeMap::new(),
            completion_observed: false,
            returning_terminal: true,
        }
    }
}

#[test]
fn captured_startup_refuses_changed_descriptors_context_owner_and_failed_target() {
    let fixture = Fixture::new();
    let capture = fixture.capture();
    assert!(capture.matches_startup(&fixture.pending));
    for mutation in 0..4 {
        let mut successor = fixture.pending.clone();
        match mutation {
            0 => successor
                .source
                .launch
                .recipe
                .arguments
                .push("changed-source".into()),
            1 => successor
                .requested
                .launch
                .recipe
                .arguments
                .push("changed-request".into()),
            2 => successor.context = Arc::new(chat::protocol::ExecutionContext::default()),
            3 => {
                successor.failed_close = Some(super::super::super::CloseTarget::from_record(
                    &fixture.record(fixture.attachment.clone()),
                ))
            }
            _ => unreachable!(),
        }
        fixture
            .host
            .lock()
            .unwrap()
            .native_actor_startups
            .insert(successor.source.id, successor.clone());
        assert!(!capture.matches_startup(&successor));
        assert!(capture.require_host(&fixture.host).is_err());
        fixture
            .host
            .lock()
            .unwrap()
            .native_actor_startups
            .insert(fixture.pending.source.id, fixture.pending.clone());
    }
    // Reusing the same Arc cannot reuse a captured owner generation.
    fixture
        .pending
        .context
        .claim_startup_owner(Arc::new(Pending));
    assert!(!capture.matches_startup(&fixture.pending));
    assert!(capture.require_host(&fixture.host).is_err());
}

#[test]
fn captured_attachment_refuses_same_actor_replacement_and_keeps_public_close_fence() {
    let fixture = Fixture::new();
    fixture.host.lock().unwrap().chats.insert(
        fixture.pending.source.id,
        fixture.record(fixture.attachment.clone()),
    );
    let capture = capture_hosted(
        &fixture.host.lock().unwrap(),
        &fixture.pending.source,
        &fixture.attachment,
    )
    .unwrap();
    let target =
        super::super::super::CloseTarget::from_record(&fixture.record(fixture.attachment.clone()));
    assert!(target.require_current(&fixture.host).is_err());
    assert!(
        target
            .require_current_with_shutdown(&fixture.host, Some(&capture))
            .is_ok()
    );
    fixture.host.lock().unwrap().chats.insert(
        fixture.pending.source.id,
        fixture.record(fixture.successor.clone()),
    );
    assert!(capture.require_host(&fixture.host).is_err());
    assert!(
        target
            .require_current_with_shutdown(&fixture.host, Some(&capture))
            .is_err()
    );
}

#[test]
fn startup_capture_never_enrolls_an_installed_unknown_or_changed_actor() {
    let fixture = Fixture::new();
    let capture = fixture.capture();
    let source = &fixture.pending.source;
    let refusal = capture
        .enroll_uninstalled(
            &fixture.host,
            &fixture.identity,
            source,
            &[],
            Err("captured unknown scope"),
            true,
        )
        .unwrap_err();
    assert!(refusal.contains("no prior durable ownership intent"));
    let mut changed = fixture.identity.clone();
    changed.instance = uuid::Uuid::new_v4();
    assert!(
        capture
            .enroll_uninstalled(
                &fixture.host,
                &changed,
                source,
                &[],
                Err("captured unknown scope"),
                false
            )
            .is_err()
    );
    let registry = crate::host::agent_execution::registry(&fixture.host).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_ownerships(None, 128)
            .unwrap()
            .is_empty()
    );
    capture
        .enroll_uninstalled(
            &fixture.host,
            &fixture.identity,
            source,
            &[],
            Err("captured unknown scope"),
            false,
        )
        .unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_ownership(fixture.identity.instance)
            .unwrap()
            .unwrap()
            .is_pending()
    );
    // Normal publication may not consume ordinary shutdown's exact evidence.
    forget_startup(&fixture.host, source, &fixture.pending.context);
    assert!(
        fixture
            .host
            .lock()
            .unwrap()
            .native_actor_startups
            .contains_key(&source.id)
    );
}

#[test]
fn ordinary_semantic_persistence_failure_retries_in_the_same_host() {
    const CHILD: &str = "CAGENTS_ORDINARY_SEMANTIC_RETRY_TEST";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = directory.path();
        for path in [
            "home/.claude",
            "run",
            "data",
            "config",
            "cache",
            "bin",
            "project",
        ] {
            std::fs::create_dir_all(root.join(path)).unwrap();
        }
        let exact = thread::current().name().unwrap().to_owned();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &exact, "--nocapture"])
            .env_clear()
            .env(CHILD, "1")
            .env(
                "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                crate::host::chat::retained_actor_tests::retained_helper_executable(),
            )
            .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
            .env("CAGENTS_HOLDER_ROOT", root)
            .env("HOME", root.join("home"))
            .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    fn wait(mut predicate: impl FnMut() -> bool, reason: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !predicate() {
            assert!(Instant::now() < deadline, "{reason}");
            thread::sleep(Duration::from_millis(20));
        }
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    install_boot_permission_provider(&root.join("bin/claude"));
    let proof = HostSessionProof {
        host_session_id: "ordinary-semantic-retry".into(),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: "same-daemon".into(),
    };
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let mut state = HostState::new(proof.clone());
    state.install_model(crate::host::model::Registry::at(&root.join("registry.sqlite3")).unwrap());
    let host = Arc::new(HostLock::new(state));
    let _notifications = crate::host::notifications::Broker::start(
        &host,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    host.lock().unwrap().notifications = Some(_notifications.handle());
    let mut source = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    source.proof = Some(proof.clone());
    let registry = crate::host::agent_execution::registry(&host).unwrap();
    registry
        .lock()
        .unwrap()
        .fresh_agent_authority(&crate::agent_execution::HostedSession::Chat(source.clone()))
        .unwrap();
    let context =
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap());
    let (mut spec, adapter) = chat::protocol::create(&source.launch).unwrap();
    spec.execution = Some(context.clone());
    let prepared = adapter.prepared_launch_identity().unwrap();
    let environment = spec.environment.clone();
    let original =
        Handle::start_remote_owned(spec, source.clone(), prepared, environment, |_| {}).unwrap();
    let identity = original.actor_identity().unwrap();
    wait(
        || original.attachment_snapshot().ready,
        "actual helper did not initialize its provider",
    );
    std::fs::write(root.join("emit-boot-permission"), "emit").unwrap();
    wait(
        || {
            Handle::inspect_remote(&identity.record, proof.clone()).is_ok_and(|inspection| {
                inspection.state.outbox_latest > inspection.state.outbox_ack
            })
        },
        "provider emitted no semantic outbox evidence",
    );
    let pending = Startup {
        source: source.clone(),
        requested: source.clone(),
        context: context.clone(),
        failed_close: None,
    };
    {
        let mut state = host.lock().unwrap();
        state
            .native_actor_startups
            .insert(source.id, pending.clone());
        state.shutting_down = true;
    }
    context.request_ordinary_cleanup();
    let capture = capture_startup(&host.lock().unwrap(), &pending).unwrap();
    let attention = crate::instance::project_dirs()
        .unwrap()
        .data_dir()
        .join("agent-attention.sqlite3");
    wait(
        || {
            rusqlite::Connection::open(&attention).is_ok_and(|db| {
                db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='seen')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap_or(false)
            })
        },
        "notification journal did not initialize",
    );
    let fault = rusqlite::Connection::open(attention).unwrap();
    fault.execute_batch("CREATE TRIGGER reject_ordinary_attention BEFORE INSERT ON seen BEGIN SELECT RAISE(ABORT, 'fixture ordinary semantic commit failure'); END;").unwrap();
    let refusal = cleanup(&host, &capture).unwrap_err();
    assert!(
        refusal.contains("fixture ordinary semantic commit failure"),
        "{refusal}"
    );
    let held = Handle::inspect_remote(&identity.record, proof.clone()).unwrap();
    assert!(held.state.outbox_latest > held.state.outbox_ack);
    assert!(
        host.lock().unwrap().chats[&source.id]
            .handle
            .is_close_only()
    );
    assert!(capture.matches_startup(&host.lock().unwrap().native_actor_startups[&source.id]));
    assert_eq!(context.outcome(), LaunchOutcome::Uncertain);
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_cursor_for_test(identity.instance)
            .unwrap()
            .is_some()
    );
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_retirement(identity.instance)
            .unwrap()
            .is_none()
    );
    fault
        .execute_batch("DROP TRIGGER reject_ordinary_attention;")
        .unwrap();
    cleanup(&host, &capture).unwrap();
    let token = context.ownership_snapshot().token.unwrap();
    wait(
        || context.complete_semantic_cleanup(token, &identity),
        "semantic settlement did not receive the original helper's actual wait proof",
    );
    assert_eq!(
        context.ownership_snapshot().original_helper,
        chat::protocol::HelperWaitProof::TeardownProved
    );
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_retirement(identity.instance)
            .unwrap()
            .is_some()
    );
    assert!(!host.lock().unwrap().chats.contains_key(&source.id));
    assert!(capture.matches_startup(&host.lock().unwrap().native_actor_startups[&source.id]));
    assert!(
        !root.join("prompts").exists(),
        "cleanup must never send provider input"
    );
}

fn install_boot_permission_provider(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, r#"#!/usr/bin/python3
import json, pathlib, select, sys
root=pathlib.Path(__file__).resolve().parent.parent
initialized=False
sent=False
def send(value): print(json.dumps(value),flush=True)
while True:
    if initialized and not sent and (root/'emit-boot-permission').exists():
        send({'type':'control_request','request_id':'ordinary-boot-permission','request':{'subtype':'can_use_tool','tool_name':'Read','tool_use_id':'ordinary-boot-read','input':{'file_path':'/fixture/read-only'}}})
        sent=True
    ready,_,_=select.select([sys.stdin],[],[],0.02)
    if not ready: continue
    line=sys.stdin.readline()
    if not line: break
    frame=json.loads(line)
    if frame.get('type')=='control_request':
        send({'type':'control_response','response':{'subtype':'success','request_id':frame['request_id'],'response':{}}})
        if frame.get('request',{}).get('subtype')=='initialize': initialized=True
    elif frame.get('type')=='user':
        with (root/'prompts').open('a') as log: log.write('prompt\n')
"#).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
