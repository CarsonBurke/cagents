//! Exercise the real provider owner and the daemon's final shutdown sequence.
use super::*;
use crate::{
    agent::Provider,
    chat::{Launch, NativeContinuity, Session},
    daemon::holder_registry::{self as registry, Kind},
    model::{HostIncarnation, HostSessionId, LaunchRecipe},
    upgrade::handoff::{Phase, Store},
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, process::Command, time::Duration};

const TEST: &str = "host::state::native_upgrade_tests::committed_upgrade_shutdown_preserves_real_native_owner_even_if_final_save_fails";
const MODE: &str = "CAGENTS_UPGRADE_NATIVE_TEST";
const THREAD: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

#[test]
fn committed_upgrade_shutdown_preserves_real_native_owner_even_if_final_save_fails() {
    match std::env::var(MODE).as_deref() {
        Ok("provider") => return provider(),
        Ok("fixture") => {}
        _ => {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let root = directory.path();
            for path in [
                "home/.codex",
                "project",
                "run",
                "data",
                "config",
                "cache",
                "bin",
            ] {
                fs::create_dir_all(root.join(path)).unwrap();
            }
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env_clear()
                .env(MODE, "fixture")
                .env("CAGENTS_UPGRADE_ROOT", root)
                .env("CAGENTS_UPGRADE_EXE", std::env::current_exe().unwrap())
                .env("HOME", root.join("home"))
                .env("CODEX_HOME", root.join("home/.codex"))
                .env("XDG_RUNTIME_DIR", root.join("run"))
                .env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_CACHE_HOME", root.join("cache"))
                .env("PATH", "/usr/bin:/bin")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated native upgrade failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_UPGRADE_ROOT").unwrap());
    let program = root.join("bin/codex");
    // exec keeps the listener in the actual owned provider process. A listener
    // in the test parent would not satisfy the native peer/ancestry proof.
    fs::write(
        &program,
        format!(
            r#"#!/bin/sh
set -eu
for arg in "$@"; do
  case "$arg" in unix://*) export CAGENTS_UPGRADE_SOCKET="${{arg#unix://}}" ;; esac
done
export {MODE}=provider
exec "$CAGENTS_UPGRADE_EXE" --exact '{TEST}' --nocapture
"#
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("native-upgrade-fixture"),
        host_incarnation: HostIncarnation::new("native-upgrade-incarnation"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
    };
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
    let cleanup_proof = Arc::new(Mutex::new(proof.clone()));
    for fail_final_save in [false, true] {
        let host = Arc::new(Mutex::new(HostState::new(proof.clone())));
        let mut source = Session {
            id: TerminalId::new(),
            launch: Launch {
                provider: Provider::Codex,
                recipe: LaunchRecipe {
                    program: program.to_string_lossy().into_owned(),
                    arguments: vec!["--remote".into(), "unix://".into()],
                    working_directory: root.join("project"),
                    environment: std::env::vars().collect(),
                    unset_environment: Default::default(),
                },
                session_id: None,
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        assert!(source.claim_owned_codex_listener(&root, &proof));
        let id = source.id;
        super::super::chat::start_execution(
            &host,
            0,
            source,
            Arc::new(crate::chat::protocol::ExecutionContext::default()),
        )
        .unwrap();
        let handle = host.lock().unwrap().chats[&id].handle.clone();
        // Always reclaim on a failed assertion; preservation is deliberately
        // exercised below, so merely dropping the handle is not cleanup.
        struct Cleanup {
            handle: Option<crate::chat::runtime::Handle>,
            source: Session,
            proof: Arc<Mutex<HostSessionProof>>,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Some(handle) = &self.handle
                    && handle.close().is_err()
                {
                    let host = Arc::new(Mutex::new(HostState::new(
                        self.proof.lock().unwrap().clone(),
                    )));
                    let _ = super::super::chat::close_retained_native(&host, &self.source);
                }
            }
        }
        let mut cleanup = Cleanup {
            handle: Some(handle.clone()),
            source: host.lock().unwrap().chats[&id].session.clone(),
            proof: cleanup_proof.clone(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(handle.native_continuity(), NativeContinuity::Durable { .. }) {
            assert!(
                Instant::now() < deadline,
                "provider did not become durable: failure={:?}, snapshot={:?}",
                handle.failure(),
                handle.snapshot(None)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(handle.confirmed_session_id().as_deref(), Some(THREAD));
        let socket = registry::socket(&registry::directory(Kind::Native).unwrap(), id);
        let record = registry::evidence(&socket, Kind::Native).unwrap().unwrap();
        assert!(record.root.is_current());
        for policy in [
            crate::update::impact::Policy::WhenIdle,
            crate::update::impact::Policy::Interrupt,
        ] {
            let preview = super::update::inspect(&host, policy, true).unwrap();
            assert!(preview.ready(), "{preview:?}");
            assert_eq!(preview.sessions.len(), 1);
            assert_eq!(
                preview.sessions[0].disposition,
                crate::update::impact::Disposition::Continues
            );
        }
        let checkpoint_directory = root.join(if fail_final_save {
            "failed-checkpoint"
        } else {
            "ready-checkpoint"
        });
        let capability = host
            .lock()
            .unwrap()
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        prepare_upgrade_with_policy(&host, capability, false, None, || {
            Store::open(&checkpoint_directory)
        })
        .unwrap();
        let store = Store::open(&checkpoint_directory).unwrap();
        let prepared = store.load().unwrap().unwrap();
        assert_eq!(prepared.chats.len(), 1);
        assert_eq!(prepared.chats[0].launch.session_id.as_deref(), Some(THREAD));
        if fail_final_save {
            fs::rename(
                &store.path,
                checkpoint_directory.join("prepared-backup.json"),
            )
            .unwrap();
            fs::create_dir(&store.path).unwrap();
        }
        // This is the helper used by the real host shutdown tail: it captures
        // upgrade intent before finish_upgrade consumes the prepared state,
        // persists Ready, then disposes of every remaining chat handle.
        let result = super::super::finish_chat_shutdown(&host);
        assert_eq!(result.is_err(), fail_final_save, "{result:?}");
        assert!(!host.lock().unwrap().has_committed_upgrade());
        assert!(
            record.root.is_current(),
            "upgrade shutdown killed retained provider (save failure={fail_final_save})"
        );
        assert!(registry::evidence(&socket, Kind::Native).unwrap().is_some());
        let (issued, _) = crate::chat::native_holder::inspect(&record, &proof).unwrap();
        let stale_source = issued
            .previous_sources
            .iter()
            .find(|source| matches!(source.native_continuity, NativeContinuity::ReconnectOnly))
            .expect("holder retains its genuinely issued pre-confirmation descriptor")
            .clone();
        if !fail_final_save {
            let ready = store.load().unwrap().unwrap();
            assert_eq!(ready.phase, Phase::Ready);
            assert_eq!(
                ready.chats[0].launch, prepared.chats[0].launch,
                "preserving a server must not rewrite its launch to a CLI resume recipe"
            );
            assert_eq!(
                serde_json::to_value(&ready.chats[0].previous_sources).unwrap(),
                serde_json::to_value(&prepared.chats[0].previous_sources).unwrap()
            );
        }
        if fail_final_save {
            let source = prepared.chats[0].source.clone();
            fs::remove_dir_all(root.join("home/.codex")).unwrap();
            // Rotate the real exclusive lease: this is a fresh daemon, not a
            // fabricated Durable flag or an in-memory cleanup cache.
            drop(lease.take());
            let next = HostSessionProof {
                host_incarnation: HostIncarnation::new("recovery"),
                ..proof.clone()
            };
            lease = Some(crate::daemon::HostLease::acquire(&paths, &next).unwrap());
            *cleanup_proof.lock().unwrap() = next.clone();
            let mut state = HostState::new(next);
            state.install_model(
                crate::host::model::Registry::at(&root.join("recovery.sqlite3")).unwrap(),
            );
            let successor = Arc::new(Mutex::new(state));
            // Startup discovery failed before the UI asked for an upgrade
            // mapping. Preserve the precise refusal for every issued alias.
            super::super::chat::restore_orphaned_native(&successor).unwrap();
            for issued_source in [&source, &stale_source] {
                let refusal = super::super::client::process_request(
                    &successor,
                    0,
                    crate::terminal::ipc::ClientRequest::ResolveUpgradeChat {
                        source: issued_source.clone(),
                    },
                )
                .unwrap_err();
                assert!(
                    refusal.message.contains("fixture history is unavailable"),
                    "{refusal:?}"
                );
            }
            let mut unrelated = source.clone();
            unrelated.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
            let refusal = super::super::client::process_request(
                &successor,
                0,
                crate::terminal::ipc::ClientRequest::ResolveUpgradeChat { source: unrelated },
            )
            .unwrap_err();
            assert!(!refusal.message.contains("fixture history is unavailable"));
            let failure = super::super::client::process_request(
                &successor,
                0,
                crate::terminal::ipc::ClientRequest::ChatStart {
                    // This real old alias predates native capability promotion.
                    // It must recover the holder, never create a second adapter.
                    session: stale_source.clone(),
                    source: None,
                    terminate_source: false,
                    execution_environment: Default::default(),
                },
            )
            .unwrap_err();
            assert!(
                failure.message.contains("fixture history is unavailable"),
                "{failure:?}"
            );
            assert!(
                record.root.is_current(),
                "failed history attachment killed retained owner"
            );
            assert!(!successor.lock().unwrap().chats.contains_key(&id));
            // Add an actual terminal view after the failed history attachment.
            // Its exact holder incarnation is committed to the native owner,
            // independently of the public descriptor's unchanged ID/recipe.
            successor.lock().unwrap().enable_holder_recovery();
            let recipe = LaunchRecipe {
                program: "/bin/cat".into(),
                arguments: Vec::new(),
                working_directory: root.join("project"),
                environment: Default::default(),
                unset_environment: Default::default(),
            };
            let terminal = crate::model::TerminalSession {
                id,
                launch: recipe.clone(),
                continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                    recipe,
                    last_exit: None,
                },
                resume_hint: None,
                transcript_path: None,
                owned_codex_listener: None,
                recovery_origin: None,
            };
            let (descriptor, terminal_handle) = spawn_terminal_view_owned(
                &successor,
                0,
                terminal,
                TerminalSpawnOptions::default(),
                None,
                false,
            )
            .unwrap();
            let current_proof = successor.lock().unwrap().proof().clone();
            let (mut holder, retained) =
                crate::chat::native_holder::adopt(&record, &source, &current_proof).unwrap();
            let mut paired_source = retained.source.clone();
            paired_source.terminal = Some(descriptor.session.clone());
            holder
                .update_source(
                    paired_source.clone(),
                    Some(descriptor.session.launch.clone()),
                    terminal_handle.holder_identity().cloned(),
                )
                .unwrap();
            let detached = holder.preserve();
            terminal_handle
                .close_with_receipt(&descriptor.session, &current_proof)
                .unwrap();
            successor.lock().unwrap().retire_terminal(id);
            let (replacement, replacement_handle) = spawn_terminal_view_owned(
                &successor,
                0,
                descriptor.session.clone(),
                TerminalSpawnOptions::default(),
                None,
                false,
            )
            .unwrap();
            assert_eq!(
                replacement.session, descriptor.session,
                "the public descriptor can repeat within one daemon"
            );
            let replacement_holder = replacement_handle.holder_identity().unwrap().clone();
            assert_ne!(
                replacement_holder,
                *terminal_handle.holder_identity().unwrap()
            );
            let refused =
                super::super::chat::close_retained_native(&successor, &paired_source).unwrap_err();
            assert!(
                refused.contains("original paired terminal holder was replaced"),
                "{refused}"
            );
            assert!(
                replacement_holder.root.is_current(),
                "stale pair cleanup stopped its replacement"
            );
            assert!(
                record.root.is_current(),
                "refused pair cleanup prematurely stopped provider"
            );
            replacement_handle
                .close_with_receipt(&replacement.session, &current_proof)
                .unwrap();
            successor.lock().unwrap().retire_terminal(id);
            // Persist only the successor authority, then close through its old
            // issued alias. A retry must reconcile the current descriptor too.
            let execution = super::super::agent_execution::registry(&successor).unwrap();
            execution
                .lock()
                .unwrap()
                .fresh_agent_authority(&crate::agent_execution::HostedSession::Chat(
                    paired_source.clone(),
                ))
                .unwrap();
            detached.close_retaining_receipt().unwrap();
            // Crash exactly after both process owners released but before the
            // native paired-complete receipt was saved or any SQL retired.
            assert!(
                crate::chat::native_holder::closed_source(&paired_source, &current_proof)
                    .unwrap()
                    .is_some_and(|(_, paired)| !paired)
            );
            drop(successor);
            drop(lease.take());
            let retry_proof = HostSessionProof {
                host_incarnation: HostIncarnation::new("paired-close-retry"),
                ..proof.clone()
            };
            lease = Some(crate::daemon::HostLease::acquire(&paths, &retry_proof).unwrap());
            *cleanup_proof.lock().unwrap() = retry_proof.clone();
            let mut state = HostState::new(retry_proof);
            state.install_model(
                crate::host::model::Registry::at(&root.join("recovery.sqlite3")).unwrap(),
            );
            let successor = Arc::new(Mutex::new(state));
            let close = crate::terminal::ipc::ClientRequest::ChatCloseRetained {
                session: paired_source,
            };
            let fault = rusqlite::Connection::open(root.join("recovery.sqlite3")).unwrap();
            fault.execute_batch("CREATE TRIGGER refuse_native_retirement BEFORE DELETE ON agent_authorities BEGIN SELECT RAISE(FAIL, 'fixture retirement unavailable'); END;").unwrap();
            let failure = super::super::client::process_request(&successor, 0, close).unwrap_err();
            assert!(
                failure.message.contains("fixture retirement unavailable"),
                "{failure:?}"
            );
            assert!(
                execution
                    .lock()
                    .unwrap()
                    .retained_agent_authority_source(id)
                    .unwrap()
                    .is_some(),
                "failed SQL retirement keeps exact authority"
            );
            fault
                .execute_batch("DROP TRIGGER refuse_native_retirement;")
                .unwrap();
            cleanup.handle.take(); // The authoritative receipt now owns cleanup proof.
            drop(successor);
            drop(lease.take());
            let last = HostSessionProof {
                host_incarnation: HostIncarnation::new("close-retry"),
                ..proof.clone()
            };
            lease = Some(crate::daemon::HostLease::acquire(&paths, &last).unwrap());
            *cleanup_proof.lock().unwrap() = last.clone();
            let mut state = HostState::new(last);
            state.install_model(
                crate::host::model::Registry::at(&root.join("recovery.sqlite3")).unwrap(),
            );
            let restarted = Arc::new(Mutex::new(state));
            assert!(matches!(
                super::super::client::process_request(
                    &restarted,
                    0,
                    crate::terminal::ipc::ClientRequest::ChatCloseRetained {
                        session: source.clone()
                    }
                )
                .unwrap()
                .0,
                crate::terminal::ipc::ServerResponse::Accepted
            ));
            assert!(
                execution
                    .lock()
                    .unwrap()
                    .retained_agent_authority_source(id)
                    .unwrap()
                    .is_none(),
                "old-alias retry must retire the exact current adopted authority too"
            );
            let mut unrelated = source;
            unrelated.launch.session_id = Some("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into());
            assert!(
                super::super::client::process_request(
                    &restarted,
                    0,
                    crate::terminal::ipc::ClientRequest::ChatCloseRetained { session: unrelated }
                )
                .is_err(),
                "a close receipt must not authorize a changed conversation"
            );
        } else {
            // Model the already-published failed local adapter from an older
            // daemon: closing it must also reclaim the independently retained
            // provider, not acknowledge only this unrelated local process.
            let local = completed_local_adapter(&root);
            let mut state = HostState::new(proof.clone());
            state.install_model(
                crate::host::model::Registry::at(&root.join("local-close.sqlite3")).unwrap(),
            );
            state.chats.insert(
                id,
                super::super::chat::Record {
                    execution_instance: uuid::Uuid::new_v4(),
                    console_recipe: None,
                    paired_terminal: None,
                    session: stale_source,
                    handle: local,
                    source_process: None,
                    subscribers: std::collections::BTreeMap::from([(0, false)]),
                    observations: Default::default(),
                    completion_observed: true,
                    returning_terminal: false,
                },
            );
            let successor = Arc::new(Mutex::new(state));
            let execution = super::super::agent_execution::registry(&successor).unwrap();
            execution
                .lock()
                .unwrap()
                .fresh_agent_authority(&crate::agent_execution::HostedSession::Chat(
                    prepared.chats[0].source.clone(),
                ))
                .unwrap();
            let fault = rusqlite::Connection::open(root.join("local-close.sqlite3")).unwrap();
            fault.execute_batch("CREATE TRIGGER refuse_local_retirement BEFORE DELETE ON agent_authorities BEGIN SELECT RAISE(FAIL, 'local retirement unavailable'); END;").unwrap();
            let failed = super::super::client::process_request(
                &successor,
                0,
                crate::terminal::ipc::ClientRequest::ChatClose { chat_id: id },
            )
            .unwrap_err();
            assert!(
                failed.message.contains("local retirement unavailable"),
                "{failed:?}"
            );
            assert!(successor.lock().unwrap().native_closing.contains_key(&id));
            assert!(successor.lock().unwrap().chats.contains_key(&id));
            assert!(registry::evidence(&socket, Kind::Native).unwrap().is_none());
            fault
                .execute_batch("DROP TRIGGER refuse_local_retirement;")
                .unwrap();
            assert!(matches!(
                super::super::client::process_request(
                    &successor,
                    0,
                    crate::terminal::ipc::ClientRequest::ChatClose { chat_id: id }
                )
                .unwrap()
                .0,
                crate::terminal::ipc::ServerResponse::ChatClosed { .. }
            ));
            assert!(!successor.lock().unwrap().chats.contains_key(&id));
            assert!(
                execution
                    .lock()
                    .unwrap()
                    .retained_agent_authority_source(id)
                    .unwrap()
                    .is_none()
            );
        }
        wait_until(
            || !record.root.is_current(),
            "explicit Close did not reap provider",
        );
        assert!(registry::evidence(&socket, Kind::Native).unwrap().is_none());
    }
}

fn wait_until(mut condition: impl FnMut() -> bool, failure: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "{failure}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn completed_local_adapter(root: &std::path::Path) -> crate::chat::runtime::Handle {
    use crate::chat::protocol::{Adapter, ProcessSpec, Step};
    struct Local;
    impl Adapter for Local {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: crate::chat::Command) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let recipe = LaunchRecipe {
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: root.into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let spec = ProcessSpec::from(&recipe);
    let mut initial = Step::default();
    let store = crate::chat::runtime::preflight(&spec, &mut initial).unwrap();
    let handle =
        crate::chat::runtime::Handle::start(spec, Box::new(Local), initial, store, None, |_| {})
            .unwrap();
    handle.close().unwrap();
    handle
}

fn provider() {
    let socket = std::env::var_os("CAGENTS_UPGRADE_SOCKET").unwrap();
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    // The retained server remains alive after the actor disconnects. Only an
    // explicit holder release should terminate this process.
    for stream in listener.incoming() {
        let mut websocket = tungstenite::accept(stream.unwrap()).unwrap();
        while let Ok(message) = websocket.read() {
            if message.is_close() {
                break;
            }
            let Ok(text) = message.to_text() else {
                continue;
            };
            let frame: Value = serde_json::from_str(text).unwrap();
            let Some(id) = frame.get("id") else { continue };
            if frame["method"] == "thread/resume"
                && !std::path::Path::new(&std::env::var("CODEX_HOME").unwrap()).is_dir()
            {
                websocket.send(tungstenite::Message::Text(json!({"id":id,"error":{"code":-32603,"message":"fixture history is unavailable"}}).to_string().into())).unwrap();
                continue;
            }
            let result = match frame["method"].as_str().unwrap() {
                "initialize" => {
                    json!({"userAgent":"cagents/0.157.1 (Linux)", "codexHome":std::env::var("CODEX_HOME").unwrap()})
                }
                "thread/start" | "thread/resume" => {
                    json!({"thread":{"id":THREAD,"canAcceptDirectInput":true,"status":{"type":"idle"}},"model":"fixture-model"})
                }
                "thread/read" => json!({"thread":{"id":THREAD,"turns":[]}}),
                "model/list" | "skills/list" => json!({"data":[]}),
                method => panic!("unexpected provider request {method}"),
            };
            if websocket
                .send(tungstenite::Message::Text(
                    json!({"id":id,"result":result}).to_string().into(),
                ))
                .is_err()
            {
                break;
            }
        }
    }
}
