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
const ROLLOUT_MISSING: &str = "no rollout found for thread id";

#[test]
fn committed_upgrade_shutdown_preserves_real_native_owner_even_if_final_save_fails() {
    match std::env::var(MODE).as_deref() {
        Ok("provider") => return provider(),
        Ok("fixture") => {}
        _ => return run_isolated(TEST, "fixture"),
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_UPGRADE_ROOT").unwrap());
    let program = install_provider(&root);
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
        let source = provider_session(&root, &program, &proof);
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
        wait_durable(&handle);
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
            super::super::chat::restore_orphaned_native(&successor, &[]).unwrap();
            for issued_source in [&source, &stale_source] {
                let refusal = super::super::client::process_request(
                    &successor,
                    0,
                    crate::terminal::ipc::ClientRequest::ResolveUpgradeChat {
                        source: issued_source.clone(),
                    },
                )
                .unwrap_err();
                assert!(refusal.message.contains(ROLLOUT_MISSING), "{refusal:?}");
            }
            let mut unrelated = source.clone();
            unrelated.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
            let refusal = super::super::client::process_request(
                &successor,
                0,
                crate::terminal::ipc::ClientRequest::ResolveUpgradeChat { source: unrelated },
            )
            .unwrap_err();
            assert!(!refusal.message.contains(ROLLOUT_MISSING));
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
            assert!(failure.message.contains(ROLLOUT_MISSING), "{failure:?}");
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

const DEGRADED_TEST: &str = "host::state::native_upgrade_tests::upgrade_restore_isolates_a_retained_conversation_whose_rollout_is_missing";

/// The failure reported in production: after a planned upgrade, Codex could no
/// longer find a retained thread's rollout in its account storage. Neither the
/// candidate daemon nor, after a rollback, its predecessor could then start at
/// all, although every carried terminal was adoptable.
#[test]
fn upgrade_restore_isolates_a_retained_conversation_whose_rollout_is_missing() {
    match std::env::var(MODE).as_deref() {
        Ok("provider") => return provider(),
        Ok("degraded") => {}
        _ => return run_isolated(DEGRADED_TEST, "degraded"),
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_UPGRADE_ROOT").unwrap());
    let program = install_provider(&root);
    let launches = || {
        fs::read_to_string(root.join("provider-launches"))
            .unwrap_or_default()
            .lines()
            .count()
    };
    // Neither a replacement conversation nor a resent prompt, ever.
    let requests = |method: &str| {
        fs::read_to_string(root.join("provider-requests"))
            .unwrap_or_default()
            .lines()
            .filter(|request| *request == method)
            .count()
    };
    let holders = root.join("holders");
    let checkpoint_directory = root.join("checkpoint");
    let empty_proc = root.join("proc");
    fs::create_dir(&empty_proc).unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let daemon = |incarnation: &str| HostSessionProof {
        host_session_id: HostSessionId::new("degraded-upgrade"),
        host_incarnation: HostIncarnation::new(incarnation),
        boot_id: crate::daemon::read_boot_id().unwrap(),
    };
    let proof = daemon("original");
    let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
    let host = Arc::new(Mutex::new(HostState::new(proof.clone())));

    // These exact processes were created only by this isolated fixture.
    #[derive(Default)]
    struct Cleanup(Vec<i32>, Option<registry::Record>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let native = self
                .1
                .iter()
                .flat_map(|record| [&record.holder, &record.root]);
            for process in native.filter(|process| process.is_current()) {
                unsafe { libc::kill(process.pid, libc::SIGKILL) };
            }
            for &pid in &self.0 {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }
    let mut cleanup = Cleanup::default();
    let mut terminals = Vec::new();
    for index in 0..2 {
        let pid_file = root.join(format!("terminal-{index}.pid"));
        let recipe = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec![
                "-c".into(),
                format!("echo $$ > '{}'; exec sleep 1000", pid_file.display()),
            ],
            working_directory: root.join("project"),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let source = crate::model::TerminalSession {
            id: TerminalId::new(),
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
        spawn_terminal(&host, 0, source.clone(), TerminalSpawnOptions::default()).unwrap();
        let mut pid = None;
        wait_until(
            || {
                pid = fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|pid| pid.trim().parse::<i32>().ok());
                pid.is_some()
            },
            "a carried shell never started",
        );
        cleanup.0.push(pid.unwrap());
        terminals.push((source.id, pid.unwrap()));
    }
    let source = provider_session(&root, &program, &proof);
    let id = source.id;
    super::super::chat::start_execution(
        &host,
        0,
        source,
        Arc::new(crate::chat::protocol::ExecutionContext::default()),
    )
    .unwrap();
    let handle = host.lock().unwrap().chats[&id].handle.clone();
    wait_durable(&handle);
    let socket = registry::socket(&registry::directory(Kind::Native).unwrap(), id);
    let record = registry::evidence(&socket, Kind::Native).unwrap().unwrap();
    cleanup.1 = Some(record.clone());
    let capability = host
        .lock()
        .unwrap()
        .shutdown_preflight(Instant::now())
        .unwrap()
        .capability;
    prepare_upgrade_with_policy(&host, capability, false, Some(&holders), || {
        Store::open(&checkpoint_directory)
    })
    .unwrap();
    super::super::finish_chat_shutdown(&host).unwrap();
    drop(host);
    let store = || Store::open(&checkpoint_directory).unwrap();
    let checkpoint = store().load().unwrap().unwrap();
    assert_eq!(checkpoint.phase, Phase::Ready);
    assert_eq!(checkpoint.carried.len(), 2);
    assert_eq!(checkpoint.chats.len(), 1);
    let saved = checkpoint.chats[0].clone();
    let relaunch = |id: TerminalId, options: TerminalSpawnOptions| {
        let recipe = LaunchRecipe::shell(root.join("project")).unwrap();
        crate::upgrade::handoff::Terminal {
            source: crate::model::TerminalSession {
                id,
                launch: recipe.clone(),
                continuity: crate::model::TerminalContinuity::DetachedSameBoot {
                    host_session_id: checkpoint.proof.host_session_id.clone(),
                    boot_id: checkpoint.proof.boot_id.clone(),
                    host_incarnation: checkpoint.proof.host_incarnation.clone(),
                },
                resume_hint: None,
                transcript_path: None,
                owned_codex_listener: None,
                recovery_origin: None,
            },
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            recipe,
            options,
        }
    };
    assert!(record.root.is_current(), "the upgrade kept its provider");
    // The provider keeps serving, but the thread's rollout is gone.
    fs::remove_dir_all(root.join("home/.codex")).unwrap();
    let mut replace_daemon = |incarnation: &str| {
        drop(lease.take());
        let proof = daemon(incarnation);
        lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
        let mut state = HostState::new(proof);
        state.install_model(
            crate::host::model::Registry::at(&root.join(format!("{incarnation}.sqlite3"))).unwrap(),
        );
        Arc::new(Mutex::new(state))
    };
    let alive = |pid: i32| {
        fs::read_to_string(format!("/proc/{pid}/comm"))
            .is_ok_and(|command| command.trim() == "sleep")
    };

    // A genuinely fatal failure still retains the whole checkpoint for a
    // rollback, and records nothing as unattached: a session the candidate
    // could not start stands in for anything its predecessor might restore.
    let mut failing = checkpoint.clone();
    failing.terminals.push(relaunch(
        TerminalId::new(),
        // No terminal can start at this size.
        TerminalSpawnOptions {
            columns: 0,
            ..TerminalSpawnOptions::default()
        },
    ));
    store().save(&failing).unwrap();
    let candidate = replace_daemon("candidate");
    let failure =
        restore_upgrade_from(&candidate, store(), &empty_proc, Some(&holders)).unwrap_err();
    assert!(failure.starts_with("Upgrade restore failed"), "{failure}");
    let retained = store().load().unwrap().unwrap();
    assert_eq!(retained.phase, Phase::Ready);
    assert_eq!(retained.chats.len(), 1);
    assert!(store().unattached().unwrap().is_empty());
    assert!(!store().unattached_path().exists());
    assert!(record.root.is_current());
    for &(_, pid) in &terminals {
        assert!(alive(pid), "a failed restore ended a carried terminal");
    }
    drop(candidate);

    // The predecessor replays that checkpoint: every carried terminal and the
    // daemon come back, and the conversation that cannot reattach is reported
    // with its exact evidence instead of taking them all down with it.
    // Had the conversation's paired view fallen back to a relaunch, as when
    // its holder cannot be carried, relaunching it would attach a new client
    // to the retained server. It is kept as evidence instead.
    let mut with_view = checkpoint.clone();
    with_view.carried.clear();
    let mut paired = relaunch(id, TerminalSpawnOptions::default());
    paired.recipe = saved
        .launch
        .terminal_recipe()
        .expect("a confirmed Codex conversation has a terminal recipe");
    paired.source.launch = paired.recipe.clone();
    with_view.chats[0].source.terminal = Some(paired.source.clone());
    with_view.terminals.push(paired.clone());
    store().save(&with_view).unwrap();
    let viewer = replace_daemon("viewer");
    restore_upgrade_from(&viewer, store(), &empty_proc, Some(&holders)).unwrap();
    assert!(
        !viewer.lock().unwrap().terminals.contains_key(&id),
        "the paired view was not relaunched against the retained server"
    );
    assert!(!viewer.lock().unwrap().chats.contains_key(&id));
    let evidence: serde_json::Value =
        serde_json::from_slice(&fs::read(store().unattached_path()).unwrap()).unwrap();
    assert_eq!(
        evidence["conversations"][0]["terminal"],
        serde_json::to_value(&paired).unwrap(),
        "the paired view's exact checkpoint entry is kept"
    );
    assert_eq!(launches(), 1);
    assert_eq!(requests("thread/start"), 1);
    assert_eq!(requests("turn/start"), 0);
    drop(viewer);

    store().save(&checkpoint).unwrap();
    // An earlier unattached conversation whose holder has since gone can no
    // longer be reattached or closed, so it stops needing attention.
    let mut departed = crate::upgrade::handoff::UnattachedChat::new(
        saved.clone(),
        "The durable native holder is unavailable".into(),
    );
    departed.summary.id = TerminalId::new();
    store()
        .record_unattached(std::slice::from_ref(&departed), &[])
        .unwrap();
    let rollback = replace_daemon("rollback");
    let refused = restore_upgrade_from(&rollback, store(), &empty_proc, Some(&holders)).unwrap();
    assert_eq!(
        refused,
        [id],
        "startup learns which conversation was refused"
    );
    assert!(store().load().unwrap().is_none(), "checkpoint was consumed");
    for &(terminal, pid) in &terminals {
        let handle = rollback.lock().unwrap().terminals[&terminal].handle.clone();
        assert!(!handle.is_completed());
        assert!(alive(pid), "the same process was adopted, not restarted");
    }
    assert!(!rollback.lock().unwrap().chats.contains_key(&id));
    assert!(record.root.is_current(), "provider ownership was kept");
    assert!(registry::evidence(&socket, Kind::Native).unwrap().is_some());
    assert_eq!(launches(), 1, "no replacement provider was started");
    assert_eq!(requests("thread/start"), 1, "no replacement conversation");
    assert_eq!(requests("turn/start"), 0, "no prompt was resent");
    let unattached = store().unattached().unwrap();
    assert_eq!(unattached.len(), 1);
    assert_eq!(unattached[0].id, id);
    assert_eq!(unattached[0].provider, Provider::Codex);
    assert_eq!(unattached[0].conversation.as_deref(), Some(THREAD));
    assert_eq!(unattached[0].working_directory, root.join("project"));
    assert!(
        unattached[0].reason.contains(ROLLOUT_MISSING),
        "{:?}",
        unattached[0].reason
    );
    let evidence: serde_json::Value =
        serde_json::from_slice(&fs::read(store().unattached_path()).unwrap()).unwrap();
    assert_eq!(
        evidence["conversations"][0]["evidence"],
        serde_json::to_value(&saved).unwrap(),
        "the exact checkpoint entry is kept"
    );
    assert!(
        evidence["conversations"][0].get("terminal").is_none(),
        "the rollback's own record replaced the earlier entry"
    );
    // A reopened card is told exactly why, for each issued descriptor.
    for issued in std::iter::once(&saved.source).chain(
        saved
            .previous_sources
            .iter()
            .map(|previous| &previous.source),
    ) {
        let refusal = super::super::client::process_request(
            &rollback,
            0,
            crate::terminal::ipc::ClientRequest::ResolveUpgradeChat {
                source: issued.clone(),
            },
        )
        .unwrap_err();
        assert!(refusal.message.contains(ROLLOUT_MISSING), "{refusal:?}");
    }

    // Once the rollout is back, the next discovery reattaches the very same
    // server, and the conversation no longer needs attention.
    fs::create_dir_all(root.join("home/.codex")).unwrap();
    // Startup's own discovery does not retry a reattachment its upgrade
    // restore just refused; that would repeat the whole confirmation wait.
    super::super::chat::restore_orphaned_native(&rollback, &refused).unwrap();
    assert!(!rollback.lock().unwrap().chats.contains_key(&id));
    assert_eq!(store().unattached().unwrap().len(), 1);
    super::super::chat::restore_orphaned_native(&rollback, &[]).unwrap();
    assert!(rollback.lock().unwrap().chats.contains_key(&id));
    assert!(store().unattached().unwrap().is_empty());
    assert!(!store().unattached_path().exists());
    let crate::terminal::ipc::ServerResponse::UpgradeChatResolved { session: resolved } =
        super::super::client::process_request(
            &rollback,
            0,
            crate::terminal::ipc::ClientRequest::ResolveUpgradeChat {
                source: saved.source.clone(),
            },
        )
        .unwrap()
        .0
    else {
        panic!("the reattached conversation did not resolve");
    };
    assert_eq!(resolved.launch.session_id.as_deref(), Some(THREAD));
    // The reopened card attaches to it as any client would, then closes it.
    let client = 1;
    super::super::client::process_request(
        &rollback,
        client,
        crate::terminal::ipc::ClientRequest::ChatAttach { session: resolved },
    )
    .unwrap();
    assert_eq!(launches(), 1);
    assert_eq!(requests("thread/start"), 1);
    assert_eq!(requests("turn/start"), 0);

    assert!(matches!(
        super::super::client::process_request(
            &rollback,
            client,
            crate::terminal::ipc::ClientRequest::ChatClose { chat_id: id }
        )
        .unwrap()
        .0,
        crate::terminal::ipc::ServerResponse::ChatClosed { .. }
    ));
    wait_until(
        || !record.root.is_current(),
        "explicit Close did not reap provider",
    );
    for &(terminal, pid) in &terminals {
        let handle = rollback.lock().unwrap().terminals[&terminal].handle.clone();
        assert_eq!(
            handle.close_checked(true),
            crate::terminal_ipc::TerminalCloseResult::Closed
        );
        wait_until(|| !alive(pid), "closing a terminal did not end it");
    }
}

/// Runs `test` again in a private runtime, data and provider home, in `mode`.
fn run_isolated(test: &str, mode: &str) {
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
        .args(["--exact", test, "--nocapture"])
        .env_clear()
        .env(MODE, mode)
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
}

/// Installs the fixture Codex executable, whose server is [`provider`].
fn install_provider(root: &std::path::Path) -> std::path::PathBuf {
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
echo launch >> "$CAGENTS_UPGRADE_ROOT/provider-launches"
export {MODE}=provider
exec "$CAGENTS_UPGRADE_EXE" --exact '{TEST}' --nocapture
"#
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    program
}

/// A fresh Codex chat served by the fixture provider, owning its listener.
fn provider_session(
    root: &std::path::Path,
    program: &std::path::Path,
    proof: &HostSessionProof,
) -> Session {
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
    assert!(source.claim_owned_codex_listener(root, proof));
    source
}

fn wait_durable(handle: &crate::chat::runtime::Handle) {
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
            // Every request the one retained server ever received, in order.
            if let Some(root) = std::env::var_os("CAGENTS_UPGRADE_ROOT") {
                use std::io::Write;
                let mut log = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(std::path::Path::new(&root).join("provider-requests"))
                    .unwrap();
                writeln!(log, "{}", frame["method"].as_str().unwrap_or_default()).unwrap();
            }
            if frame["method"] == "thread/resume"
                && !std::path::Path::new(&std::env::var("CODEX_HOME").unwrap()).is_dir()
            {
                // Codex's own refusal once account storage no longer holds the rollout.
                let message = format!("no rollout found for thread id {THREAD}");
                websocket
                    .send(tungstenite::Message::Text(
                        json!({"id":id,"error":{"code":-32603,"message":message}})
                            .to_string()
                            .into(),
                    ))
                    .unwrap();
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
                "thread/turns/list" => json!({"data":[],"nextCursor":null}),
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
