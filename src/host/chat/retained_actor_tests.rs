//! Real execed holder/provider and kernel daemon-lease qualification. This
//! isolates HOME/runtime/data; native UI qualification separately kills cagentsd.
use super::*;
use crate::{
    agent_execution::{
        Authority, Destination, Origin, Spec, State, Submit, Surface, Target as ExecutionTarget,
    },
    model::{HostIncarnation, HostSessionId, ProjectKind},
    workspace_model::ModelEdit,
};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

const MODE: &str = "CAGENTS_CLAUDE_HOLDER_TEST";

pub(super) fn retained_helper_executable() -> std::path::PathBuf {
    let executable = std::path::PathBuf::from(
        std::env::var_os("CAGENTS_TEST_RETAINED_ACTOR_HELPER")
            .expect("real retained-actor fixtures require the same-head cagentsd executable"),
    );
    assert!(
        executable.is_absolute(),
        "retained-actor helper path must be absolute"
    );
    executable
}

// Exercise the actual shutdown tail, not a hand-built PreparedUpgrade. The
// provider, holder, checkpoint and successor all retain one issued session.
#[test]
fn committed_upgrade_tail_preserves_exact_claude_actor_and_successor_attachment() {
    retained_upgrade_tail(false);
}

#[test]
fn failed_final_upgrade_save_preserves_exact_claude_actor_and_successor_attachment() {
    retained_upgrade_tail(true);
}

fn retained_upgrade_tail(fail_final_save: bool) {
    use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
    use crate::upgrade::handoff::{Phase, Store};
    const CHILD: &str = "CAGENTS_RETAINED_UPGRADE_TEST";
    const SCOPE: &str = "CAGENTS_RETAINED_UPGRADE_AUTHORITY_PRESENCE";
    const ROOT: &str = "CAGENTS_RETAINED_UPGRADE_ROOT";
    let name = if fail_final_save {
        "failed_final_upgrade_save_preserves_exact_claude_actor_and_successor_attachment"
    } else {
        "committed_upgrade_tail_preserves_exact_claude_actor_and_successor_attachment"
    };
    let _helper = retained_helper_executable();
    if std::env::var(CHILD).as_deref() != Ok(name) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
        for path in [
            "home/.claude",
            "run",
            "data",
            "config",
            "cache",
            "bin",
            "project",
            "tmp",
        ] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        let mut command = CommandSpec::new(std::env::current_exe().unwrap());
        // Inherit the caller's opaque delegation and actual validated helper;
        // neither value is copied into a marker or replaced with User authority.
        for (key, _) in std::env::vars_os() {
            if key != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV)
                && key != std::ffi::OsStr::new("CAGENTS_TEST_RETAINED_ACTOR_HELPER")
            {
                command = command.env_remove(key);
            }
        }
        command = command
            .args([
                "--exact",
                &format!("host::chat::retained_actor_tests::{name}"),
                "--nocapture",
                "--test-threads=1",
            ])
            .cwd(root)
            .env(CHILD, name)
            .env(
                SCOPE,
                if Authority::from_environment() != Authority::default() {
                    "present"
                } else {
                    "absent"
                },
            )
            .env(ROOT, root)
            .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
            .env("HOME", root.join("home"))
            .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("TMPDIR", root.join("tmp"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .timeout(Duration::from_secs(90))
            .output_limits(256 * 1024, 256 * 1024);
        let result = SystemProcessExecutor.execute(&command, &CancellationToken::new());
        let (stdout, stderr) = match &result {
            Ok(output) => (&output.stdout, &output.stderr),
            Err(error) => (&error.stdout, &error.stderr),
        };
        fs::write(root.join("child-stdout"), stdout).unwrap();
        fs::write(root.join("child-stderr"), stderr).unwrap();
        // Retain original evidence until the external command guardian has
        // settled every admitted owner, including after a failed child.
        let retained = directory.keep();
        let output = result.expect("bounded retained-upgrade child must settle");
        assert!(
            output.success,
            "retained upgrade child failed (evidence {}):\n{}\n{}",
            retained.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        println!("retained upgrade fixture evidence: {}", retained.display());
        return;
    }
    let expected_scope = match std::env::var(SCOPE).unwrap().as_str() {
        "present" => true,
        "absent" => false,
        _ => panic!("invalid inherited scope marker"),
    };
    assert_eq!(
        Authority::from_environment() != Authority::default(),
        expected_scope
    );
    let root = std::path::PathBuf::from(std::env::var_os(ROOT).unwrap());
    install_provider(&root.join("bin/claude"));
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = |incarnation| HostSessionProof {
        host_session_id: HostSessionId::new("retained-upgrade-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new(incarnation),
    };
    let first = proof("first");
    let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &first).unwrap());
    let registry_path = root.join("registry.sqlite3");
    let mut registry = super::super::model::Registry::at(&registry_path).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: root.join("project"),
            name: "Retained upgrade fixture".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let accepted = registry
        .submit_agent(
            &Origin::User,
            &Submit {
                name: None,
                destination: Destination::New(Spec {
                    provider: Provider::Claude,
                    account: Some(crate::account::Account::system(Provider::Claude).id()),
                    target: ExecutionTarget {
                        repository: root.join("project"),
                        directory: root.join("project"),
                        placement: None,
                    },
                    surface: Surface::Chat,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                }),
                prompt: "One exact slow turn".into(),
                idempotency_key: "retained-upgrade-one-turn".into(),
            },
            1,
        )
        .unwrap();
    let mut state = HostState::new(first.clone());
    state.install_model(registry);
    let host = Arc::new(HostLock::new(state));
    let notifications = super::super::notifications::Broker::start(
        &host,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    host.lock().unwrap().notifications = Some(notifications.handle());
    let source = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    let id = source.id;
    start_execution(
        &host,
        0,
        source,
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
    )
    .unwrap();
    let handle = host.lock().unwrap().chats[&id].handle.clone();
    let identity = handle
        .actor_identity()
        .expect("real execed retained Claude actor");
    let active_proof = Arc::new(std::sync::Mutex::new(first.clone()));
    struct Cleanup {
        owner: Option<crate::daemon::holder_registry::Record>,
        proof: Arc<std::sync::Mutex<HostSessionProof>>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let Some(owner) = &self.owner else { return };
            // Preservation deliberately makes the old proxy unsuitable for
            // Close. Reauthenticate only this exact fixture owner for cleanup.
            let proof = self.proof.lock().unwrap().clone();
            let result = Handle::inspect_remote(owner, proof.clone())
                .and_then(|inspection| {
                    Handle::adopt_remote(owner, inspection.source, proof, |_| {})
                })
                .and_then(|handle| {
                    handle.close()?;
                    handle.finish_retained_close()
                });
            if let Err(error) = result {
                eprintln!("retained-upgrade cleanup remains unconfirmed: {error}");
            }
        }
    }
    let mut cleanup = Cleanup {
        owner: Some(identity.record.clone()),
        proof: active_proof.clone(),
    };
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let source = host.lock().unwrap().chats[&id].session.clone();
    registry
        .lock()
        .unwrap()
        .bind_agent(
            accepted.agent.id,
            None,
            crate::agent_execution::HostedSession::Chat(source),
        )
        .unwrap();
    handle
        .command_for_run(
            accepted.run.id,
            chat::Command::Prompt {
                text: "One exact slow turn".into(),
                attachments: Vec::new(),
                mode: chat::PromptMode::Send,
            },
        )
        .unwrap();
    wait_until(
        || {
            root.join("provider-done").exists()
                && registry
                    .lock()
                    .unwrap()
                    .agent_run(accepted.run.id)
                    .unwrap()
                    .state
                    == State::Succeeded
                && handle.is_idle()
                && handle
                    .uncommitted_outcomes()
                    .is_ok_and(|outcomes| outcomes.is_empty())
        },
        "accepted Claude turn and semantic ACK did not settle",
    );
    let session_id = handle
        .confirmed_session_id()
        .expect("provider confirmed issued session");
    assert!(matches!(
        handle.native_continuity(),
        chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
    ));
    let control_gate = host.lock().unwrap().control_gate.clone();
    let control = control_gate.write().unwrap();
    let capability = host
        .lock()
        .unwrap()
        .shutdown_preflight(Instant::now())
        .unwrap()
        .capability;
    super::super::state::prepare_upgrade(&host, capability, false).unwrap();
    drop(control);
    let store = Store::discover().unwrap();
    let prepared = store.load().unwrap().unwrap();
    assert_eq!(prepared.phase, Phase::Prepared);
    assert_eq!(prepared.chats.len(), 1);
    assert_eq!(
        prepared.chats[0].launch.session_id.as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(
        prepared.chats[0].source.native_continuity,
        handle.native_continuity()
    );
    let prepared_bytes = fs::read(&store.path).unwrap();
    if fail_final_save {
        fs::rename(&store.path, root.join("prepared-backup.json")).unwrap();
        fs::create_dir(&store.path).unwrap();
    }
    let result = super::super::finish_chat_shutdown(&host);
    if fail_final_save {
        let error = result.expect_err("actual Ready publication must fail");
        assert!(
            error.contains(&std::io::Error::from_raw_os_error(libc::EISDIR).to_string()),
            "must fail the actual checkpoint rename, not an earlier actor operation: {error}"
        );
        assert!(store.path.is_dir());
        assert_eq!(
            fs::read(root.join("prepared-backup.json")).unwrap(),
            prepared_bytes
        );
    } else {
        assert!(
            result.is_ok(),
            "retained Claude must not receive a forbidden post-preserve Close: {result:?}"
        );
        let ready = store.load().unwrap().unwrap();
        assert_eq!(ready.phase, Phase::Ready);
        assert_eq!(ready.chats[0].launch, prepared.chats[0].launch);
    }
    assert!(!host.lock().unwrap().has_committed_upgrade());
    assert!(identity.record.root.is_current() && identity.record.holder.is_current());
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let old_host = Arc::downgrade(&host);
    let proxy_alive = handle.resource_lifetime();
    drop(handle);
    drop(notifications);
    drop(host);
    wait_until(
        || old_host.strong_count() == 0 && !proxy_alive(),
        "old daemon/proxy did not detach",
    );
    drop(lease.take());
    let next = proof("successor");
    lease = Some(crate::daemon::HostLease::acquire(&paths, &next).unwrap());
    *active_proof.lock().unwrap() = next.clone();
    let mut state = HostState::new(next);
    state.enable_holder_recovery();
    state.install_model(super::super::model::Registry::at(&registry_path).unwrap());
    let successor = Arc::new(HostLock::new(state));
    let notifications = super::super::notifications::Broker::start(
        &successor,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    successor.lock().unwrap().notifications = Some(notifications.handle());
    super::native_recovery::restore(&successor, 0, &prepared.chats[0].source).unwrap();
    let handle = successor.lock().unwrap().chats[&id].handle.clone();
    assert_eq!(handle.actor_identity(), Some(identity.clone()));
    assert_eq!(
        handle.confirmed_session_id().as_deref(),
        Some(session_id.as_str())
    );
    handle.set_visible(true);
    wait_until(
        || {
            handle
                .snapshot(None)
                .is_some_and(|snapshot| snapshot.items.len() >= 40)
        },
        "successor failed to hydrate retained transcript",
    );
    let snapshot = handle.snapshot(None).unwrap();
    assert_retained_copy_sources(&handle, &snapshot);
    let transcript = snapshot
        .items
        .iter()
        .filter_map(|item| match &item.content {
            chat::ItemContent::Text {
                text,
                thinking: false,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    for index in 0..40 {
        assert_eq!(transcript.matches(&format!("FRAME_{index:03}|")).count(), 1);
    }
    let successor_registry = super::super::agent_execution::registry(&successor).unwrap();
    assert_eq!(
        successor_registry
            .lock()
            .unwrap()
            .agent_run(accepted.run.id)
            .unwrap()
            .state,
        State::Succeeded
    );
    super::super::agent_execution::process(
        &successor,
        Authority::default(),
        crate::agent_execution::Operation::Close(accepted.agent.id),
    )
    .unwrap();
    wait_until(
        || !identity.record.root.is_current() && !identity.record.holder.is_current(),
        "normal owning Close left original provider/helper alive",
    );
    assert!(!successor.lock().unwrap().chats.contains_key(&id));
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1,
        "upgrade/recovery must not replay the accepted prompt"
    );
    cleanup.owner = None;
    drop(notifications);
    drop(lease);
}

#[test]
fn slow_turn_keeps_exact_provider_and_frames_across_daemon_lease_rotation() {
    retained_turn_final_action("close");
}

#[test]
fn retained_terminal_handoff_proves_original_console_cleanup() {
    retained_turn_final_action("handoff");
}

#[test]
fn retained_checked_terminal_close_proves_original_console_cleanup() {
    retained_turn_final_action("checked");
}

#[test]
fn retained_completed_terminal_release_proves_original_console_cleanup() {
    retained_turn_final_action("release");
}

fn retained_turn_final_action(action: &str) {
    let _helper_executable = retained_helper_executable();
    if std::env::var(MODE).as_deref() != Ok("fixture") {
        let name = match action {
            "close" => "slow_turn_keeps_exact_provider_and_frames_across_daemon_lease_rotation",
            "handoff" => "retained_terminal_handoff_proves_original_console_cleanup",
            "checked" => "retained_checked_terminal_close_proves_original_console_cleanup",
            "release" => "retained_completed_terminal_release_proves_original_console_cleanup",
            _ => unreachable!(),
        };
        let test = format!("host::chat::retained_actor_tests::{name}");
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
            fs::create_dir_all(root.join(path)).unwrap();
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env_clear()
            .env(
                "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                retained_helper_executable(),
            )
            .env(MODE, "fixture")
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
            "isolated retained actor qualification failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    install_provider(&root.join("bin/claude"));
    // Only this lease-rotation fixture opts into the third-frame barrier.
    fs::write(root.join("lease-third-frame-gate"), b"hold").unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = |incarnation| HostSessionProof {
        host_session_id: HostSessionId::new("retained-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new(incarnation),
    };
    let first = proof("first");
    let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &first).unwrap());
    let path = root.join("registry.sqlite3");
    let mut registry = super::super::model::Registry::at(&path).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: root.join("project"),
            name: "Retained actor fixture".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let accepted = registry
        .submit_agent(
            &Origin::User,
            &Submit {
                name: None,
                destination: Destination::New(Spec {
                    provider: Provider::Claude,
                    account: Some(crate::account::Account::system(Provider::Claude).id()),
                    target: ExecutionTarget {
                        repository: root.join("project"),
                        directory: root.join("project"),
                        placement: None,
                    },
                    surface: Surface::Chat,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                }),
                prompt: "One exact slow turn".into(),
                idempotency_key: "one-slow-turn".into(),
            },
            1,
        )
        .unwrap();
    let mut state = HostState::new(first.clone());
    state.install_model(registry);
    let host = Arc::new(HostLock::new(state));
    let notifications = super::super::notifications::Broker::start(
        &host,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    host.lock().unwrap().notifications = Some(notifications.handle());
    let source = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    let id = source.id;
    let context =
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap());
    start_execution(&host, 0, source, context).unwrap();
    let handle = host.lock().unwrap().chats[&id].handle.clone();
    let identity = handle
        .actor_identity()
        .expect("the test opts into a real retained actor");
    let active_proof = Arc::new(std::sync::Mutex::new(first.clone()));
    struct Cleanup {
        owner: crate::daemon::holder_registry::Record,
        pair: Option<crate::daemon::holder_registry::Record>,
        proof: Arc<std::sync::Mutex<HostSessionProof>>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let proof = self.proof.lock().unwrap().clone();
            if let Ok(inspection) = Handle::inspect_remote(&self.owner, proof.clone())
                && let Ok(handle) =
                    Handle::adopt_remote(&self.owner, inspection.source, proof, |_| {})
                && handle.close().is_ok()
                && handle.finish_retained_close().is_ok()
                && self
                    .pair
                    .as_ref()
                    .is_none_or(|pair| !pair.root.is_current() && !pair.holder.is_current())
            {
                return;
            }
            // Only this fixture's exact process incarnations; no name/group sweep.
            for owner in std::iter::once(&self.owner).chain(self.pair.iter()) {
                for process in [&owner.root, &owner.holder] {
                    if process.is_current() {
                        unsafe {
                            libc::kill(process.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
    }
    let mut cleanup = Cleanup {
        owner: identity.record.clone(),
        pair: None,
        proof: active_proof.clone(),
    };
    // Fail the independent notification transaction after the execution cursor
    // can commit. Adoption must replay that attention before acknowledging it.
    let attention_path = crate::instance::project_dirs()
        .unwrap()
        .data_dir()
        .join("agent-attention.sqlite3");
    wait_until(
        || {
            attention_path.exists()
                && rusqlite::Connection::open(&attention_path).is_ok_and(|db| {
                    db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='seen')",
                [], |row| row.get::<_, bool>(0),
            ).unwrap_or(false)
                })
        },
        "notification journal did not initialize",
    );
    let attention_fault = rusqlite::Connection::open(&attention_path).unwrap();
    attention_fault.execute_batch("CREATE TRIGGER reject_retained_attention BEFORE INSERT ON seen BEGIN SELECT RAISE(ABORT, 'fixture attention commit failure'); END;").unwrap();
    // A real holder-owned thin console shares this native actor's exact ID.
    // Adoption retains the authenticated source in its private cleanup owner;
    // native recovery must separately bind that exact worker and project its
    // current daemon proof, without replacing the original PTY process.
    host.lock().unwrap().enable_holder_recovery();
    let console_recipe = crate::model::LaunchRecipe {
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: root.join("project"),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    host.lock()
        .unwrap()
        .chats
        .get_mut(&id)
        .unwrap()
        .console_recipe = Some(console_recipe.clone());
    let terminal =
        crate::terminal_ipc::TerminalDescriptor::relaunchable(id, console_recipe.clone(), None)
            .session;
    let (descriptor, console) = super::super::state::spawn_terminal_view_owned(
        &host,
        0,
        terminal,
        crate::terminal_ipc::TerminalSpawnOptions::default(),
        None,
        true,
    )
    .unwrap();
    let pair = console
        .holder_identity()
        .expect("fixture requires actual terminal holder")
        .clone();
    cleanup.pair = Some(pair.clone());
    let mut paired_source = handle
        .actor_identity()
        .and_then(|owner| Handle::inspect_remote(&owner.record, first.clone()).ok())
        .unwrap()
        .source;
    paired_source.terminal = Some(descriptor.session.clone());
    handle
        .update_native_source(
            paired_source.clone(),
            Some(console_recipe.clone()),
            Some(pair.clone()),
        )
        .unwrap();
    {
        let mut state = host.lock().unwrap();
        let record = state.chats.get_mut(&id).unwrap();
        record.session = paired_source;
        record.paired_terminal = Some(console.clone());
    }
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let hosted_source = host.lock().unwrap().chats[&id].session.clone();
    registry
        .lock()
        .unwrap()
        .bind_agent(
            accepted.agent.id,
            None,
            crate::agent_execution::HostedSession::Chat(hosted_source.clone()),
        )
        .unwrap();
    handle
        .command_for_run(
            accepted.run.id,
            chat::Command::Prompt {
                text: "One exact slow turn".into(),
                attachments: Vec::new(),
                mode: chat::PromptMode::Send,
            },
        )
        .unwrap();
    wait_until_with_diagnostic(
        || {
            fs::read_to_string(root.join("produced"))
                .unwrap_or_default()
                .lines()
                .count()
                >= 3
        },
        "provider never began its slow turn",
        || slow_turn_diagnostic(&root, &handle),
    );
    assert_eq!(
        fs::read_to_string(root.join("produced"))
            .unwrap()
            .lines()
            .count(),
        3,
        "the opt-in provider gate must hold after exactly the third frame",
    );
    // Provider writes and HOST semantic commits are independent. Hold the
    // actual turn open while awaiting the real transactional run/cursor pair;
    // neither this predicate nor its polling drives the semantic broker.
    let commit_deadline = Instant::now() + Duration::from_secs(15);
    wait_until_before_with_diagnostic(
        commit_deadline,
        || {
            let registry = registry.lock().unwrap();
            registry.agent_run(accepted.run.id).unwrap().state == State::Running
                && registry
                    .native_actor_cursor_for_test(identity.instance)
                    .unwrap()
                    .is_some_and(|sequence| sequence > 0)
        },
        "provider began but its Running state and semantic cursor did not commit",
        || {
            running_commit_diagnostic(
                &root,
                &host,
                &handle,
                &registry,
                &identity,
                accepted.run.id,
                &first,
            )
        },
    );
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .agent_run(accepted.run.id)
            .unwrap()
            .state,
        State::Running
    );
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_cursor_for_test(identity.instance)
            .unwrap()
            .is_some_and(|sequence| sequence > 0)
    );
    // Invalid hybrids must still be refused by the actual current helper;
    // recovery must project a genuine issued descriptor rather than relax it.
    let inspection = Handle::inspect_remote(&identity.record, first.clone()).unwrap();
    let mut invalid = inspection.source.clone();
    invalid.launch.session_id = handle.confirmed_session_id();
    assert!(invalid.launch.session_id.is_some());
    invalid.native_continuity = chat::NativeContinuity::ReconnectOnly;
    let refusal = handle
        .update_native_source(invalid, Some(console_recipe.clone()), Some(pair.clone()))
        .unwrap_err();
    assert!(refusal.contains("issued recipe identity"), "{refusal}");
    let mut wrong_uuid = inspection.source;
    wrong_uuid.native_continuity = handle.native_continuity();
    wrong_uuid.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
    assert!(
        handle
            .update_native_source(wrong_uuid, Some(console_recipe.clone()), Some(pair.clone()))
            .is_err()
    );
    let old_host = Arc::downgrade(&host);
    let old_proxy_alive = handle.resource_lifetime();
    console.preserve_for_restart().unwrap();
    drop(console);
    drop(handle);
    drop(notifications);
    drop(host);
    drop(lease.take());
    wait_until(
        || old_host.strong_count() == 0 && !old_proxy_alive(),
        "the original HOST/proxy did not detach before provider release",
    );
    fs::write(root.join("lease-third-frame-release"), b"continue").unwrap();
    // The actual reducer, accepted command UUID, Store and protocol pipes stay
    // in the same helper while there is no daemon lease or attached proxy.
    wait_until(
        || root.join("provider-done").exists(),
        "provider did not complete while detached",
    );
    assert!(
        identity.record.root.is_current(),
        "the original provider must still own its stream"
    );
    let next = proof("second");
    lease = Some(crate::daemon::HostLease::acquire(&paths, &next).unwrap());
    *active_proof.lock().unwrap() = next.clone();
    let mut state = HostState::new(next);
    state.enable_holder_recovery();
    state.install_model(super::super::model::Registry::at(&path).unwrap());
    let successor = Arc::new(HostLock::new(state));
    let _notifications = super::super::notifications::Broker::start(
        &successor,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    successor.lock().unwrap().notifications = Some(_notifications.handle());
    super::super::state::restore_orphaned_holders(&successor).unwrap();
    let restored_console = {
        let state = successor.lock().unwrap();
        let terminal = &state.terminals[&id];
        // Orphan discovery appends the actual inspected holder source to these
        // issued aliases. Keep that proof for cleanup, independently of UI state.
        assert_eq!(Some(&terminal.session), terminal.upgrade_sources.last());
        assert_eq!(
            terminal.session.continuity.live_proof(),
            Some(first.clone())
        );
        assert_eq!(terminal.handle.holder_identity(), Some(&pair));
        assert!(!terminal.handle.is_completed());
        assert!(!state.chats.contains_key(&id));
        assert!(!state.chat_bindings.contains_key(&id));
        assert!(!state.native_actor_startups.contains_key(&id));
        let descriptor = state.descriptor(0, id).unwrap();
        assert!(!descriptor.attached);
        assert!(matches!(
            descriptor.session.continuity,
            crate::model::TerminalContinuity::DetachedSameBoot { .. }
        ));
        assert_eq!(
            descriptor.session.continuity.live_proof().as_ref(),
            Some(state.proof())
        );
        terminal.handle.clone()
    };
    // One real restore attempt exposes its exact refusal in the surviving raw
    // panic log; orphan discovery otherwise logs and intentionally skips errors.
    super::native_recovery::restore(&successor, 0, &hosted_source).unwrap();
    {
        let state = successor.lock().unwrap();
        let record = &state.chats[&id];
        assert!(state.terminals[&id].handle.same_worker(&restored_console));
        assert_eq!(state.terminals[&id].handle.holder_identity(), Some(&pair));
        assert!(
            pair.root.is_current(),
            "the exact original console process must survive"
        );
        assert_eq!(
            record.session.terminal.as_ref(),
            Some(&state.native_terminal_projection(id).unwrap())
        );
        assert!(
            record
                .paired_terminal
                .as_ref()
                .unwrap()
                .same_worker(&state.terminals[&id].handle)
        );
    }
    let handle = successor.lock().unwrap().chats[&id].handle.clone();
    assert_eq!(
        handle.actor_identity(),
        Some(identity.clone()),
        "adoption must retain both helper and original provider incarnations"
    );
    let retained = retained_actor::retained_runs(&successor);
    let registry = super::super::agent_execution::registry(&successor).unwrap();
    let mut replayed_evidence = None;
    wait_until(
        || {
            let page = handle.uncommitted_outcomes().unwrap();
            let Some(entry) = page.iter().find(|entry| {
                matches!(
                    &entry.update,
                    Update::Attention(_) | Update::AttentionContext { .. }
                )
            }) else {
                return false;
            };
            let committed = registry
                .lock()
                .unwrap()
                .native_actor_cursor_for_test(identity.instance)
                .unwrap()
                .is_some_and(|cursor| cursor >= entry.sequence);
            if committed {
                let evidence = match &entry.update {
                    Update::Attention(evidence) | Update::AttentionContext { evidence, .. } => {
                        evidence
                    }
                    _ => unreachable!(),
                };
                replayed_evidence = Some(serde_json::to_string(evidence).unwrap());
            }
            committed
        },
        "the failed notification write must retain attention despite a committed registry cursor",
    );
    attention_fault
        .execute_batch("DROP TRIGGER reject_retained_attention;")
        .unwrap();
    drop(attention_fault);
    registry
        .lock()
        .unwrap()
        .interrupt_unproven_agent_runs_except(2, &retained)
        .unwrap();
    wait_until(
        || {
            registry
                .lock()
                .unwrap()
                .agent_run(accepted.run.id)
                .unwrap()
                .state
                == State::Succeeded
        },
        "retained final outcome did not commit as Succeeded",
    );
    let run = registry.lock().unwrap().agent_run(accepted.run.id).unwrap();
    for index in 0..40 {
        assert_eq!(
            run.result
                .text
                .matches(&format!("FRAME_{index:03}|"))
                .count(),
            1,
            "missing or duplicate frame {index}"
        );
    }
    handle.set_visible(true);
    wait_until(
        || {
            handle
                .snapshot(None)
                .is_some_and(|snapshot| snapshot.items.len() >= 40)
        },
        "retained transcript failed to hydrate",
    );
    let snapshot = handle.snapshot(None).unwrap();
    assert_retained_copy_sources(&handle, &snapshot);
    let transcript = snapshot
        .items
        .iter()
        .filter_map(|item| match &item.content {
            chat::ItemContent::Text {
                text,
                thinking: false,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    for index in 0..40 {
        assert_eq!(transcript.matches(&format!("FRAME_{index:03}|")).count(), 1);
    }
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1,
        "adoption must never resend the accepted prompt"
    );
    wait_until(
        || handle.uncommitted_outcomes().unwrap().is_empty(),
        "all semantic stores must commit before helper ACK",
    );
    let attention = rusqlite::Connection::open(&attention_path).unwrap();
    let committed: u64 = attention
        .query_row(
            "SELECT count(*) FROM seen WHERE evidence=?1",
            [replayed_evidence.unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        committed, 1,
        "the retained attention must commit once before helper ACK"
    );
    let mut expected_retirement = successor.lock().unwrap().chats[&id].session.clone();
    expected_retirement.launch.session_id = handle.confirmed_session_id();
    let proof = successor.lock().unwrap().proof().clone();
    if action == "close" {
        super::super::agent_execution::process(
            &successor,
            Authority::default(),
            crate::agent_execution::Operation::Close(accepted.agent.id),
        )
        .unwrap();
    } else if action == "handoff" {
        process_inner(
            &successor,
            0,
            ClientRequest::ChatAttach {
                session: expected_retirement.clone(),
            },
        )
        .unwrap();
        let other_transition = {
            let mut state = successor.lock().unwrap();
            capacity::Reservation::closing(&successor, &mut state, &expected_retirement).unwrap()
        };
        assert!(
            process_inner(
                &successor,
                0,
                ClientRequest::ChatReturnTerminal { chat_id: id }
            )
            .is_err(),
            "a second handoff cannot stop an actor or spawn a writer through another transition's reservation"
        );
        assert!(identity.record.root.is_current() && pair.root.is_current());
        assert!(!handle.is_completed());
        assert_eq!(successor.lock().unwrap().terminals.len(), 1);
        drop(other_transition);
        let ServerResponse::ChatTerminal {
            session, snapshot, ..
        } = process_inner(
            &successor,
            0,
            ClientRequest::ChatReturnTerminal { chat_id: id },
        )
        .unwrap()
        else {
            panic!("expected exact terminal handoff");
        };
        assert_eq!(snapshot.session_id, expected_retirement.launch.session_id);
        assert_eq!(
            session.launch,
            expected_retirement
                .launch
                .terminal_recipe_with_permission_mode(snapshot.permission_mode.as_deref())
                .unwrap()
        );
        assert_ne!(session.id, id);
        {
            let state = successor.lock().unwrap();
            assert!(!state.chats.contains_key(&id));
            assert!(
                !state.terminals.contains_key(&id),
                "handoff must remove the exact original console"
            );
            assert!(state.terminals.contains_key(&session.id));
        }
        let ServerResponse::ChatTerminal {
            session: repeated, ..
        } = process_inner(
            &successor,
            0,
            ClientRequest::ChatReturnTerminal { chat_id: id },
        )
        .unwrap()
        else {
            panic!("expected idempotent handoff receipt");
        };
        assert_eq!(repeated, session);
        assert_eq!(successor.lock().unwrap().terminals.len(), 1);
        let replacement = successor.lock().unwrap().terminals[&session.id]
            .handle
            .clone();
        replacement.close_with_receipt(&session, &proof).unwrap();
        successor.lock().unwrap().retire_terminal(session.id);
        process_inner(
            &successor,
            0,
            ClientRequest::ChatCommitTerminal {
                terminal_id: session.id,
            },
        )
        .unwrap();
    } else {
        super::super::client::process_request(
            &successor,
            0,
            ClientRequest::Attach {
                terminal_id: id,
                after_generation: 0,
                after_event_sequence: 0,
            },
        )
        .unwrap();
        let request = if action == "checked" {
            ClientRequest::CloseTerminalChecked {
                terminal_id: id,
                allow_active: true,
            }
        } else {
            let worker = successor.lock().unwrap().terminals[&id].handle.clone();
            assert_eq!(
                worker.close_checked(true),
                crate::terminal_ipc::TerminalCloseResult::Closed
            );
            let session = successor.lock().unwrap().descriptor(0, id).unwrap().session;
            ClientRequest::ReleaseCompletedTerminal {
                session,
                allow_active: true,
            }
        };
        let (response, _, _) =
            super::super::client::process_request(&successor, 0, request).unwrap();
        assert!(matches!(response, ServerResponse::TerminalCloseChecked {
            terminal_id, outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
        } if terminal_id == id));
        assert!(!successor.lock().unwrap().chats.contains_key(&id));
    }
    wait_until_with_diagnostic(
        || {
            !identity.record.holder.is_current()
                && !identity.record.root.is_current()
                && !pair.holder.is_current()
                && !pair.root.is_current()
        },
        "explicit close left a native or paired owner",
        || close_owner_diagnostic(&identity.record, &pair),
    );
    let retirement = registry
        .lock()
        .unwrap()
        .native_actor_retirement(identity.instance)
        .unwrap()
        .expect("owning Close must durably retire the exact actor");
    assert_eq!(retirement.owner(), &identity.record);
    let canonical = retirement.source().clone();
    assert_eq!(canonical, expected_retirement);
    assert!(canonical.terminal.is_some());
    let confirmed = retirement.confirmed_session().unwrap().to_owned();
    assert_eq!(
        canonical.launch.session_id.as_deref(),
        Some(confirmed.as_str())
    );
    let ServerResponse::ChatResumePrepared { session } = process_inner(
        &successor,
        0,
        ClientRequest::ChatPrepareResume {
            source: canonical.clone(),
        },
    )
    .expect("ordinary hosted Close must prove paired cleanup before disk resume") else {
        panic!("expected exact disk-resume preparation");
    };
    assert_ne!(session.id, canonical.id);
    assert_eq!(session.launch, canonical.launch);
    assert!(session.terminal.is_none());
    assert!(successor.lock().unwrap().chats.is_empty());
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    drop(lease);
}

// Failure-only inspection of the four process incarnations admitted by this
// fixture. No discovery, RPC, signal, wait, account read or ownership mutation.
fn close_process_diagnostic(
    role: &str,
    expected: &crate::daemon::holder_registry::Process,
    custody_contract: &str,
) -> serde_json::Value {
    use std::io::Read;
    let observed = fs::File::open(format!("/proc/{}/stat", expected.pid))
        .and_then(|file| {
            let mut bytes = Vec::new();
            file.take(4096).read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .map(|bytes| {
            let text = String::from_utf8_lossy(&bytes);
            let fields = text
                .rsplit_once(')')
                .map(|(_, tail)| tail.split_whitespace().collect::<Vec<_>>())
                .unwrap_or_default();
            let start =
                crate::model::agent_recovery::process_identity(&bytes).map(|(_, start)| start);
            serde_json::json!({
                "state": fields.first().copied(),
                "parent_pid": fields.get(1).and_then(|value| value.parse::<i32>().ok()),
                "observed_start": start,
                "matches_admitted_start": start == Some(expected.start),
            })
        })
        .unwrap_or_else(|error| serde_json::json!({"read_error": error.to_string()}));
    serde_json::json!({
        "role": role,
        "admitted_pid": expected.pid,
        "admitted_start": expected.start,
        "observed_stat": observed,
        "source_custody_contract_not_a_live_child_handle_observation": custody_contract,
    })
}

fn close_owner_diagnostic(
    native: &crate::daemon::holder_registry::Record,
    paired: &crate::daemon::holder_registry::Record,
) -> String {
    serde_json::json!({
        "fixture_pid": std::process::id(),
        "scope": "One bounded stat read per admitted role after the unchanged close deadline; no RPC, signal, wait or scan. Missing or mismatched identities are reported without reuse as authority.",
        "roles": [
            close_process_diagnostic("native helper", &native.holder,
                "The original helper Child was assigned to the cagents-actor-owner wait thread in this fixture process."),
            close_process_diagnostic("native provider", &native.root,
                "The retained helper runtime owns the provider Child."),
            close_process_diagnostic("paired holder", &paired.holder,
                "This fixture process spawned the original terminal Child, then detached its Holder. The adopted Holder has no original Child; this describes source custody, not a newly observed wait handle."),
            close_process_diagnostic("paired root", &paired.root,
                "The paired holder owns its PTY root Child; successful close receipt requires that exact root reaped."),
        ],
    }).to_string()
}

fn sigkill_retirement_diagnostic(
    identity: &chat::runtime::ActorIdentity,
    proof: &HostSessionProof,
) -> String {
    let error_category = |mut error: String| {
        if error.len() > 512 {
            let mut end = 512;
            while !error.is_char_boundary(end) {
                end -= 1;
            }
            error.truncate(end);
        }
        serde_json::json!({ "kind": "Err", "error": error })
    };
    // Existing typed read-only evidence readers: no hydration, helper RPC,
    // refresh, retry, ACK or change to retirement authority.
    let retired = match Handle::retired_actor_receipt(&identity.record, proof) {
        Ok(Some(_)) => serde_json::json!({ "kind": "Some" }),
        Ok(None) => serde_json::json!({ "kind": "None" }),
        Err(error) => error_category(error),
    };
    let pending = match crate::daemon::holder_registry::retirement_pending(&identity.record) {
        Ok(value) => serde_json::json!({ "kind": "Ok", "pending": value }),
        Err(error) => error_category(error.to_string()),
    };
    serde_json::json!({
        "successor_pid": std::process::id(),
        "successor_parent_pid": unsafe { libc::getppid() },
        "scope": "Failure-only bounded exact-role observations after owning Close returned its original error; existing deadlines are unchanged. No RPC, signal, wait, retry, scan or raw receipt contents. These categories grant no cleanup authority and cannot distinguish every None cause.",
        "typed_retired_receipt": retired,
        "diagnostic_only_discovery_retirement_pending": pending,
        "roles": [
            close_process_diagnostic("native helper", &identity.record.holder,
                "The original helper Child and cagents-actor-owner wait thread belonged to the killed source process. This successor has no original Child; the observed parent is reported separately."),
            close_process_diagnostic("native provider", &identity.record.root,
                "The retained helper owns the provider Child and its descendant reaping; observed state and parent are separate evidence."),
        ],
    }).to_string()
}

fn wait_until(mut condition: impl FnMut() -> bool, reason: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "{reason}");
        thread::sleep(Duration::from_millis(20));
    }
}

// Capture only cached metadata and bounded private fixture files. In particular,
// a timeout must not initiate an RPC or read transcript/prompt contents.
fn slow_turn_diagnostic(root: &Path, handle: &Handle) -> String {
    use std::io::Read;
    let state = handle.attachment_snapshot();
    let pending = handle.pending_retained_inputs();
    let read = |name: &str, limit: u64| {
        fs::File::open(root.join(name))
            .and_then(|file| {
                let mut bytes = Vec::new();
                file.take(limit).read_to_end(&mut bytes)?;
                Ok(bytes)
            })
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    let count = |name: &str| read(name, 4096).map(|text| text.lines().count());
    let error = state
        .error
        .as_deref()
        .map(|text| &text[..text.floor_char_boundary(2048)]);
    format!(
        "cached actor: generation={} ready={} busy={} waiting={} exited={} requests={} failure={error:?}; pending input count={} first sequences={:?}; prompts(lines, first 4096 bytes)={:?}; produced(lines, first 4096 bytes)={:?}; provider trace(first 8192 bytes)={:?}",
        state.generation,
        state.ready,
        state.busy,
        state.waiting,
        state.exited,
        state.requests.len(),
        pending.len(),
        pending
            .iter()
            .take(16)
            .map(|input| input.sequence)
            .collect::<Vec<_>>(),
        count("prompts"),
        count("produced"),
        read("provider-trace.jsonl", 8192),
    )
}

// Failure-only typed reads: do not acknowledge, adopt, refresh the registry,
// or otherwise substitute for the product's independent outcome broker.
fn running_commit_diagnostic(
    root: &Path,
    host: &SharedHost,
    handle: &Handle,
    registry: &super::super::agent_execution::SharedRegistry,
    identity: &chat::runtime::ActorIdentity,
    run: crate::model::RunId,
    proof: &HostSessionProof,
) -> String {
    let bounded_error = |error: String| error[..error.floor_char_boundary(512)].to_owned();
    let (state, cursor) = {
        let registry = registry.lock().unwrap();
        (
            registry
                .agent_run(run)
                .map(|run| run.state)
                .map_err(&bounded_error),
            registry
                .native_actor_cursor_for_test(identity.instance)
                .map_err(&bounded_error),
        )
    };
    let expected_source = host
        .lock()
        .unwrap()
        .chats
        .get(&identity.record.terminal)
        .map(|record| record.session.clone());
    let source = expected_source.map(|source| {
        (
            source.id,
            source.launch.session_id,
            source.proof.as_ref() == Some(proof),
        )
    });
    // The absolute probe deadline has expired. Never create a new RPC budget
    // here: helper journal/source/inspection/adoption evidence stays unobserved.
    format!(
        "run={run} state={state:?} cursor={cursor:?}; cached HOST source(id, session, proof matches fixture)={source:?}; helper ACK/latest/source/inspection/adoption=unobserved (deadline exhausted; no RPC); {}",
        slow_turn_diagnostic(root, handle),
    )
}

fn assert_retained_copy_sources(handle: &Handle, snapshot: &chat::Snapshot) {
    assert_eq!(
        snapshot.items.len(),
        snapshot.text_copy_sources.conversation.len()
    );
    let mut checked = 0;
    for (item, source) in snapshot
        .items
        .iter()
        .zip(&snapshot.text_copy_sources.conversation)
    {
        let chat::ItemContent::Text { text: visible, .. } = &item.content else {
            continue;
        };
        let source = source
            .as_ref()
            .expect("retained helper must preserve logical text Copy");
        let mut copied = String::new();
        loop {
            let page = handle.text_copy(source, copied.len() as u64).unwrap();
            assert_eq!(&page.source, source);
            assert_eq!(page.offset, copied.len() as u64);
            assert!(!page.text.is_empty() || page.done);
            copied.push_str(&page.text);
            if page.done {
                break;
            }
        }
        assert_eq!(copied.len() as u64, source.bytes);
        assert!(copied.contains(visible.as_str()));
        checked += 1;
    }
    assert!(
        checked >= 40,
        "all actual streamed frames must remain copyable"
    );
}

fn wait_until_with_diagnostic(
    condition: impl FnMut() -> bool,
    reason: &str,
    diagnostic: impl FnOnce() -> String,
) {
    wait_until_before_with_diagnostic(
        Instant::now() + Duration::from_secs(15),
        condition,
        reason,
        diagnostic,
    );
}

fn wait_until_before_with_diagnostic(
    deadline: Instant,
    mut condition: impl FnMut() -> bool,
    reason: &str,
    diagnostic: impl FnOnce() -> String,
) {
    loop {
        if Instant::now() >= deadline {
            panic!("{reason}: {}", diagnostic());
        }
        let satisfied = condition();
        if Instant::now() >= deadline {
            panic!("{reason}: {}", diagnostic());
        }
        if satisfied {
            return;
        }
        thread::sleep(
            Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

pub(super) fn install_provider(path: &Path) {
    fs::write(path, r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
scriptroot=pathlib.Path(__file__).resolve().parent.parent
tracepath=scriptroot/'provider-trace.jsonl'
def trace(value):
    # Fixed allowlist below; never record prompt bodies, argv, or the environment.
    data=(json.dumps(value,ensure_ascii=True)[:2047]+'\n').encode()
    try:
        with tracepath.open('ab') as log:
            if log.tell()+len(data)<=8192: log.write(data)
    except OSError:
        pass  # Diagnostic I/O must not change the provider's protocol behavior.
trace({'event':'bootstrap','HOME':os.environ.get('HOME'),'CLAUDE_CONFIG_DIR':os.environ.get('CLAUDE_CONFIG_DIR'),'cwd':os.getcwd(),'scriptroot':str(scriptroot)})
root=pathlib.Path(os.environ['HOME']).parent
trace({'event':'marker-root','markerroot':str(root)})
session=next((arg.split('=',1)[1] for arg in sys.argv[1:] if arg.startswith('--session-id=')), 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa')
def send(value): print(json.dumps(value),flush=True)
for line in sys.stdin:
    frame=json.loads(line)
    trace({'event':'input','type':str(frame.get('type'))[:96],'uuid':str(frame.get('uuid'))[:96]})
    if frame.get('type')=='control_request':
        send({'type':'control_response','response':{'subtype':'success','request_id':frame['request_id'],'response':{}}})
    elif frame.get('type')=='user':
        with (root/'prompts').open('a') as log: log.write('prompt\n')
        received=frame['uuid']
        send({'type':'user','uuid':received,'session_id':session,'parent_tool_use_id':None,'message':{'id':'native-'+received,'role':'user','content':[{'type':'text','text':'One exact slow turn'}]}})
        send({'type':'command_lifecycle','command_uuid':received,'state':'started','session_id':session,'uuid':'lifecycle-'+received})
        for index in range(40):
            send({'type':'assistant','uuid':f'frame-{index}','message':{'id':f'frame-{index}','role':'assistant','content':[{'type':'text','text':f'FRAME_{index:03}|'}]}})
            with (root/'produced').open('a') as log: log.write(str(index)+'\n')
            gate=root/'lease-third-frame-gate'
            if index==2 and gate.exists():
                deadline=time.monotonic()+45
                while gate.exists() and not (root/'lease-third-frame-release').exists():
                    if time.monotonic()>=deadline:
                        trace({'event':'third-frame-gate-timeout'})
                        raise RuntimeError('private fixture third-frame gate timed out')
                    time.sleep(0.02)
            time.sleep(0.04)
        send({'type':'result','uuid':'final','subtype':'success','is_error':False,'session_id':session,'usage':{'input_tokens':7,'output_tokens':40}})
        (root/'provider-done').write_text('done')
"#).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

/// This boundary kills a separate host owner process with SIGKILL. Unlike a
/// lease-rotation-only fixture, no destructor or old proxy can help recovery.
// These types belong only to the two SIGKILL fixtures. In particular, the
// ordinary libtest process never changes its process-wide subreaper setting.
#[cfg(target_os = "linux")]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SigkillCase {
    id: uuid::Uuid,
    selected_test: String,
    semantic_held: bool,
    deadline_ns: u64,
}
#[cfg(target_os = "linux")]
impl SigkillCase {
    const CLEANUP_NS: u64 = 30_000_000_000;
    fn now_ns() -> u64 {
        let mut value: libc::timespec = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) },
            0
        );
        value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
    }
    fn remaining(&self, cleanup: bool) -> Duration {
        Duration::from_nanos(
            self.deadline_ns
                .saturating_sub(Self::now_ns())
                .saturating_sub(if cleanup { 0 } else { Self::CLEANUP_NS }),
        )
    }
    fn wait(&self, maximum: Duration, mut condition: impl FnMut() -> bool, reason: &str) {
        self.wait_before(
            Instant::now() + maximum.min(self.remaining(false)),
            &mut condition,
            reason,
        );
    }
    fn wait_before(&self, deadline: Instant, mut condition: impl FnMut() -> bool, reason: &str) {
        let deadline = deadline.min(Instant::now() + self.remaining(false));
        loop {
            assert!(
                Instant::now() < deadline,
                "{reason}: shared SIGKILL fixture deadline"
            );
            if condition() {
                assert!(
                    Instant::now() < deadline,
                    "{reason}: shared SIGKILL fixture deadline"
                );
                return;
            }
            thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn sigkill_read<T: serde::de::DeserializeOwned>(path: &Path) -> T {
    sigkill_try_read(path).unwrap()
}
#[cfg(target_os = "linux")]
fn sigkill_try_read<T: serde::de::DeserializeOwned>(path: &Path) -> std::io::Result<T> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut bytes = Vec::new();
    file.take(128 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 128 * 1024 {
        return Err(std::io::Error::other("bounded SIGKILL fixture receipt"));
    }
    serde_json::from_slice(&bytes).map_err(std::io::Error::other)
}
#[cfg(target_os = "linux")]
fn sigkill_write(path: &Path, value: &impl serde::Serialize) {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let bytes = serde_json::to_vec(value).unwrap();
    assert!(bytes.len() <= 128 * 1024, "bounded SIGKILL fixture marker");
    let temporary = path.with_file_name(format!(".sigkill-marker-{}", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        // Publish complete bytes atomically while refusing an existing final
        // path. Pollers must never observe a half-written create_new marker.
        fs::hard_link(&temporary, path)
    })();
    let _ = fs::remove_file(&temporary);
    result.unwrap();
}
#[cfg(target_os = "linux")]
fn sigkill_output(path: &Path) -> String {
    use std::io::Read;
    let mut bytes = Vec::new();
    if let Ok(file) = fs::File::open(path) {
        file.take(1024 * 1024 + 1).read_to_end(&mut bytes).unwrap();
    }
    assert!(
        bytes.len() <= 1024 * 1024,
        "private fixture output exceeded 1 MiB"
    );
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(target_os = "linux")]
fn sigkill_child(root: &Path, role: &str, case: &SigkillCase) -> std::process::Command {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", &case.selected_test, "--nocapture"])
        .env_clear()
        .env(
            "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
            retained_helper_executable(),
        )
        .env("CAGENTS_RETAINED_HOST_PROCESS_ROLE", role)
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
        );
    command
}

#[cfg(target_os = "linux")]
struct SigkillChild {
    child: std::process::Child,
    case: SigkillCase,
    stdout: std::path::PathBuf,
    stderr: std::path::PathBuf,
}
#[cfg(target_os = "linux")]
impl SigkillChild {
    fn spawn(command: std::process::Command, root: &Path, role: &str, case: &SigkillCase) -> Self {
        Self::try_spawn(command, root, role, case).unwrap()
    }
    fn try_spawn(
        mut command: std::process::Command,
        root: &Path,
        role: &str,
        case: &SigkillCase,
    ) -> std::io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let stdout = root.join(format!("{role}.stdout"));
        let stderr = root.join(format!("{role}.stderr"));
        let output = |path: &Path| {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
        };
        let child = command
            .stdout(output(&stdout)?)
            .stderr(output(&stderr)?)
            .spawn()?;
        Ok(Self {
            child,
            case: case.clone(),
            stdout,
            stderr,
        })
    }
    fn poll(&mut self) -> Option<std::process::ExitStatus> {
        for path in [&self.stdout, &self.stderr] {
            assert!(
                fs::metadata(path).unwrap().len() <= 1024 * 1024,
                "bounded private fixture output"
            );
        }
        self.child.try_wait().unwrap()
    }
    fn stop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        // This Child still owns its wait status; it cannot name a reused PID.
        let _ = self.child.kill();
        while !self.case.remaining(true).is_zero() {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        eprintln!(
            "exact fixture Child {} did not settle before cleanup deadline",
            self.child.id()
        );
    }
}
#[cfg(target_os = "linux")]
impl Drop for SigkillChild {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(target_os = "linux")]
struct SigkillProcess {
    identity: crate::daemon::holder_registry::Process,
    pidfd: std::os::fd::OwnedFd,
    collected: bool,
}
#[cfg(target_os = "linux")]
impl SigkillProcess {
    fn capture(identity: &crate::daemon::holder_registry::Process) -> Self {
        Self::try_capture(identity).unwrap()
    }
    fn try_capture(identity: &crate::daemon::holder_registry::Process) -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;
        if identity.pid <= 0 || identity.start == 0 || !identity.is_current() {
            return Err(std::io::Error::other(
                "fixture process is no longer the admitted incarnation",
            ));
        }
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
        if !identity.is_current() {
            return Err(std::io::Error::other(
                "fixture process changed while opening pidfd",
            ));
        }
        Ok(Self {
            identity: identity.clone(),
            pidfd,
            collected: false,
        })
    }
    fn parent(&self) -> Option<i32> {
        use std::io::Read;
        let file = fs::File::open(format!("/proc/{}/stat", self.identity.pid)).ok()?;
        let mut bytes = Vec::new();
        file.take(4096).read_to_end(&mut bytes).ok()?;
        let (_, start) = crate::model::agent_recovery::process_identity(&bytes)?;
        if start != self.identity.start {
            return None;
        }
        let text = std::str::from_utf8(&bytes).ok()?;
        text.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    }
    fn exited(&self) -> bool {
        use std::os::fd::AsRawFd;
        let mut descriptor = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(unsafe { libc::poll(&mut descriptor, 1, 0) } >= 0);
        descriptor.revents & libc::POLLIN != 0
    }
    fn reap(&mut self) -> Option<i32> {
        if self.collected {
            return None;
        }
        assert_eq!(
            self.parent(),
            Some(std::process::id() as i32),
            "exact admitted process did not reparent to the dedicated coordinator"
        );
        let mut status = 0;
        let result = unsafe { libc::waitpid(self.identity.pid, &mut status, libc::WNOHANG) };
        assert!(
            result >= 0,
            "exact wait custody lost (including ECHILD): {}",
            std::io::Error::last_os_error()
        );
        if result == 0 {
            return None;
        }
        assert_eq!(result, self.identity.pid);
        self.collected = true;
        assert!(
            !self.identity.is_current(),
            "collected exact process is still current"
        );
        Some(status)
    }
    fn kill(&self) {
        use std::os::fd::AsRawFd;
        if self.collected {
            return;
        }
        // A captured pidfd, never a numeric lookup, grants this cleanup signal.
        let _ = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
    }
    fn cleanup_reap(&mut self) {
        if !self.collected && self.parent() == Some(std::process::id() as i32) {
            let mut status = 0;
            let result = unsafe { libc::waitpid(self.identity.pid, &mut status, libc::WNOHANG) };
            if result == self.identity.pid {
                self.collected = true;
            } else if result < 0 {
                eprintln!(
                    "exact cleanup wait failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SigkillMarker {
    case: uuid::Uuid,
    selected_test: String,
    helper: crate::daemon::holder_registry::Process,
    provider: crate::daemon::holder_registry::Process,
}
#[cfg(target_os = "linux")]
impl SigkillMarker {
    fn for_actor(case: &SigkillCase, identity: &chat::runtime::ActorIdentity) -> Self {
        Self {
            case: case.id,
            selected_test: case.selected_test.clone(),
            helper: identity.record.holder.clone(),
            provider: identity.record.root.clone(),
        }
    }
    fn verify(&self, case: &SigkillCase, identity: &chat::runtime::ActorIdentity) {
        assert_eq!(self.case, case.id);
        assert_eq!(self.selected_test, case.selected_test);
        assert_eq!(self.helper, identity.record.holder);
        assert_eq!(self.provider, identity.record.root);
    }
}

#[cfg(target_os = "linux")]
#[derive(serde::Serialize, serde::Deserialize)]
struct SigkillAdmission {
    case: uuid::Uuid,
    selected_test: String,
    coordinator: crate::daemon::holder_registry::Process,
}

#[cfg(target_os = "linux")]
#[derive(serde::Serialize, serde::Deserialize)]
struct SigkillReceipt {
    case: uuid::Uuid,
    identity: chat::runtime::ActorIdentity,
    agent: crate::model::AgentId,
    run: crate::model::RunId,
    session: TerminalId,
}
#[cfg(target_os = "linux")]
struct SigkillStartupGuard(Option<Handle>);
#[cfg(target_os = "linux")]
impl Drop for SigkillStartupGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take()
            && let Err(error) = handle.close().and_then(|()| handle.finish_retained_close())
        {
            eprintln!("exact startup actor cleanup was not proved: {error}");
        }
    }
}

// Drop order is explicit: stop original source/successor Children before any
// private cleanup lease, then signal/reap only the captured helper/provider.
#[cfg(target_os = "linux")]
struct SigkillCoordinator {
    case: SigkillCase,
    root: std::path::PathBuf,
    source: SigkillChild,
    successor: Option<SigkillChild>,
    helper: Option<SigkillProcess>,
    provider: Option<SigkillProcess>,
    successful: bool,
}
#[cfg(target_os = "linux")]
impl Drop for SigkillCoordinator {
    fn drop(&mut self) {
        if self.successful {
            return;
        }
        if let Some(successor) = &mut self.successor {
            successor.stop();
        }
        self.source.stop();
        let receipt = sigkill_try_read::<SigkillReceipt>(&self.root.join("owner-receipt.json"))
            .ok()
            .filter(|receipt| receipt.case == self.case.id);
        if let Some(receipt) = &receipt {
            // A capture failure must not disable exact record cleanup. These
            // identities come from the same complete typed private receipt.
            if self.helper.is_none() {
                self.helper = SigkillProcess::try_capture(&receipt.identity.record.holder).ok();
            }
            if self.provider.is_none() {
                self.provider = SigkillProcess::try_capture(&receipt.identity.record.root).ok();
            }
        } else {
            // A hard kill before any completed handoff cannot manufacture
            // unknown process identities. This is a failed/uncertain case;
            // the already-owned outer supervisor must contain the family and
            // reach ECHILD before releasing its slot, never accept this path.
            eprintln!(
                "SIGKILL fixture failed before typed actor handoff; cleanup is uncertain, outer family containment required"
            );
        }
        if receipt.is_some() && !self.case.remaining(true).is_zero() {
            match SigkillChild::try_spawn(
                sigkill_child(&self.root, "cleanup", &self.case),
                &self.root,
                "cleanup",
                &self.case,
            ) {
                Ok(mut cleanup) => {
                    let deadline =
                        Instant::now() + Duration::from_secs(10).min(self.case.remaining(true));
                    while Instant::now() < deadline {
                        let oversized =
                            [&cleanup.stdout, &cleanup.stderr].into_iter().any(|path| {
                                fs::metadata(path)
                                    .map_or(true, |metadata| metadata.len() > 1024 * 1024)
                            });
                        if oversized || cleanup.child.try_wait().ok().flatten().is_some() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    cleanup.stop();
                }
                Err(error) => eprintln!("private cleanup Child could not start: {error}"),
            }
        }
        // No waitpid(-1): provider may become this coordinator's child only if
        // its original helper dies on a failed path. Retain that exact custody.
        if let Some(provider) = &self.provider {
            provider.kill();
        }
        if let Some(helper) = &self.helper {
            helper.kill();
        }
        while !self.case.remaining(true).is_zero() {
            let mut pending = false;
            for process in [&mut self.helper, &mut self.provider].into_iter().flatten() {
                process.cleanup_reap();
                pending |= process.identity.is_current();
            }
            if !pending {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        for process in [&self.helper, &self.provider].into_iter().flatten() {
            if process.identity.is_current() {
                eprintln!(
                    "exact fixture owner remains after bounded cleanup: {:?}",
                    process.identity
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn slow_turn_survives_sigkill_of_the_host_owner_process() {
    sigkill_host_fixture(false);
}

#[cfg(target_os = "linux")]
#[test]
fn semantic_retirement_precedes_original_parent_wait_after_host_sigkill() {
    sigkill_host_fixture(true);
}

#[cfg(target_os = "linux")]
fn sigkill_host_fixture(semantic_held: bool) {
    let _helper_executable = retained_helper_executable();
    const ROLE: &str = "CAGENTS_RETAINED_HOST_PROCESS_ROLE";
    if std::env::var_os(ROLE).is_none() {
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
            fs::create_dir_all(root.join(path)).unwrap();
        }
        install_provider(&root.join("bin/claude"));
        let case = SigkillCase {
            id: uuid::Uuid::new_v4(),
            selected_test: thread::current().name().unwrap().to_owned(),
            semantic_held,
            deadline_ns: SigkillCase::now_ns() + 150_000_000_000,
        };
        sigkill_write(&root.join("sigkill-case.json"), &case);
        // Reexec first: PR_SET_CHILD_SUBREAPER is process-wide and must never
        // affect unrelated tests running beside this parent in libtest.
        let mut coordinator = SigkillChild::spawn(
            sigkill_child(root, "coordinator", &case),
            root,
            "coordinator",
            &case,
        );
        let status = loop {
            if let Some(status) = coordinator.poll() {
                break status;
            }
            assert!(
                !case.remaining(true).is_zero(),
                "dedicated SIGKILL coordinator exceeded shared budget"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(
            status.success(),
            "dedicated SIGKILL coordinator failed:\n{}\n{}",
            sigkill_output(&coordinator.stdout),
            sigkill_output(&coordinator.stderr)
        );
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    let case: SigkillCase = sigkill_read(&root.join("sigkill-case.json"));
    assert_eq!(case.selected_test, thread::current().name().unwrap());
    assert_eq!(case.semantic_held, semantic_held);
    let role = std::env::var(ROLE).unwrap();
    if role == "coordinator" {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
            0
        );
        sigkill_write(
            &root.join("coordinator-admission.json"),
            &SigkillAdmission {
                case: case.id,
                selected_test: case.selected_test.clone(),
                coordinator: crate::daemon::holder_registry::Process::read(
                    std::process::id() as i32
                )
                .unwrap(),
            },
        );
        let source_ready_deadline =
            Instant::now() + Duration::from_secs(20).min(case.remaining(false));
        let source = SigkillChild::spawn(
            sigkill_child(&root, "source", &case),
            &root,
            "source",
            &case,
        );
        let mut coordinator = SigkillCoordinator {
            case: case.clone(),
            root: root.clone(),
            source,
            successor: None,
            helper: None,
            provider: None,
            successful: false,
        };
        case.wait_before(
            source_ready_deadline,
            || {
                assert!(
                    coordinator.source.poll().is_none(),
                    "source host exited before its exact receipt: {}",
                    sigkill_output(&coordinator.source.stderr)
                );
                root.join("owner-receipt.json").exists()
            },
            "source host never published its exact receipt",
        );
        let receipt: SigkillReceipt = sigkill_read(&root.join("owner-receipt.json"));
        assert_eq!(receipt.case, case.id);
        coordinator.helper = Some(SigkillProcess::capture(&receipt.identity.record.holder));
        coordinator.provider = Some(SigkillProcess::capture(&receipt.identity.record.root));
        sigkill_write(
            &root.join("actor-custody.json"),
            &SigkillMarker::for_actor(&case, &receipt.identity),
        );
        case.wait_before(
            source_ready_deadline,
            || {
                assert!(
                    coordinator.source.poll().is_none(),
                    "source host failed before the provider began"
                );
                root.join("source-ready.json").exists()
            },
            "provider never began before source SIGKILL",
        );
        let ready: SigkillMarker = sigkill_read(&root.join("source-ready.json"));
        ready.verify(&case, &receipt.identity);
        let source_identity =
            crate::daemon::holder_registry::Process::read(coordinator.source.child.id() as i32)
                .unwrap();
        coordinator.source.child.kill().unwrap();
        let killed = loop {
            if let Some(status) = coordinator.source.poll() {
                break status;
            }
            assert!(
                !case.remaining(false).is_zero(),
                "source SIGKILL wait exceeded shared budget"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(killed.signal(), Some(libc::SIGKILL));
        assert!(
            !source_identity.is_current(),
            "exact source must actually be killed and collected"
        );
        case.wait(
            Duration::from_secs(15),
            || coordinator.helper.as_ref().unwrap().parent() == Some(std::process::id() as i32),
            "exact helper did not reparent to the dedicated coordinator",
        );
        case.wait(
            Duration::from_secs(15),
            || root.join("provider-done").exists(),
            "provider did not finish while no host owner existed",
        );
        coordinator.successor = Some(SigkillChild::spawn(
            sigkill_child(&root, "successor", &case),
            &root,
            "successor",
            &case,
        ));
        let mut helper_collected = false;
        let status = loop {
            assert!(
                !case.remaining(false).is_zero(),
                "successor exceeded shared SIGKILL budget"
            );
            let release = if case.semantic_held {
                if root.join("semantic-retirement.json").exists() {
                    let marker: SigkillMarker =
                        sigkill_read(&root.join("semantic-retirement.json"));
                    marker.verify(&case, &receipt.identity);
                    true
                } else {
                    false
                }
            } else {
                true
            };
            let helper = coordinator.helper.as_mut().unwrap();
            if !helper_collected && release && helper.exited() {
                if let Some(status) = helper.reap() {
                    assert!(libc::WIFEXITED(status));
                    assert_eq!(
                        libc::WEXITSTATUS(status),
                        73,
                        "exact helper must prove its existing teardown contract"
                    );
                    helper_collected = true;
                    sigkill_write(
                        &root.join("helper-reaped.json"),
                        &SigkillMarker::for_actor(&case, &receipt.identity),
                    );
                }
            }
            if let Some(status) = coordinator.successor.as_mut().unwrap().poll() {
                break status;
            }
            thread::sleep(Duration::from_millis(10));
        };
        let successor = coordinator.successor.as_ref().unwrap();
        assert!(
            status.success(),
            "successor failed (coordinator_pid={}, source_pid={}):\n{}\n{}",
            std::process::id(),
            coordinator.source.child.id(),
            sigkill_output(&successor.stdout),
            sigkill_output(&successor.stderr)
        );
        assert!(
            helper_collected,
            "successor cannot pass without exact original-parent collection"
        );
        assert!(
            !receipt.identity.record.holder.is_current()
                && !receipt.identity.record.root.is_current()
        );
        coordinator.successful = true;
        return;
    }
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("sigkill-host-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new(&role),
    };
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    if role == "cleanup" {
        let receipt: SigkillReceipt = sigkill_read(&root.join("owner-receipt.json"));
        assert_eq!(receipt.case, case.id);
        // Only the exact actor already captured in this fresh case. Failure
        // signals and adopted waits remain with the coordinator's pidfds.
        if let Ok(inspection) = Handle::inspect_remote(&receipt.identity.record, proof.clone())
            && let Ok(handle) =
                Handle::adopt_remote(&receipt.identity.record, inspection.source, proof, |_| {})
        {
            let _ = handle.close();
            let _ = handle.finish_retained_close();
        }
        return;
    }
    let path = root.join("registry.sqlite3");
    let mut registry = super::super::model::Registry::at(&path).unwrap();
    let accepted = if role == "source" {
        registry
            .apply(&ModelEdit::OpenProject {
                repository: root.join("project"),
                name: "Killed host fixture".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        Some(
            registry
                .submit_agent(
                    &Origin::User,
                    &Submit {
                        name: None,
                        destination: Destination::New(Spec {
                            provider: Provider::Claude,
                            account: Some(crate::account::Account::system(Provider::Claude).id()),
                            target: ExecutionTarget {
                                repository: root.join("project"),
                                directory: root.join("project"),
                                placement: None,
                            },
                            surface: Surface::Chat,
                            model: None,
                            permission_mode: None,
                            reasoning_effort: None,
                        }),
                        prompt: "One exact killed-host turn".into(),
                        idempotency_key: "one-killed-host-turn".into(),
                    },
                    1,
                )
                .unwrap(),
        )
    } else {
        None
    };
    let mut state = HostState::new(proof);
    state.install_model(registry);
    let host = Arc::new(HostLock::new(state));
    let _notifications = super::super::notifications::Broker::start(
        &host,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    host.lock().unwrap().notifications = Some(_notifications.handle());
    if let Some(accepted) = accepted {
        let session = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            root.join("project"),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
        let id = session.id;
        start_execution(
            &host,
            0,
            session,
            Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
        )
        .unwrap();
        let (source, handle) = {
            let state = host.lock().unwrap();
            (
                state.chats[&id].session.clone(),
                state.chats[&id].handle.clone(),
            )
        };
        let mut startup = SigkillStartupGuard(Some(handle.clone()));
        let receipt = SigkillReceipt {
            case: case.id,
            identity: handle.actor_identity().unwrap(),
            agent: accepted.agent.id,
            run: accepted.run.id,
            session: id,
        };
        sigkill_write(&root.join("owner-receipt.json"), &receipt);
        case.wait(
            Duration::from_secs(15),
            || root.join("actor-custody.json").exists(),
            "coordinator did not acknowledge exact helper/provider custody",
        );
        let custody: SigkillMarker = sigkill_read(&root.join("actor-custody.json"));
        custody.verify(&case, &receipt.identity);
        startup.0 = None;
        super::super::agent_execution::registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .bind_agent(
                accepted.agent.id,
                None,
                crate::agent_execution::HostedSession::Chat(source),
            )
            .unwrap();
        handle
            .command_for_run(
                accepted.run.id,
                chat::Command::Prompt {
                    text: "One exact killed-host turn".into(),
                    attachments: Vec::new(),
                    mode: chat::PromptMode::Send,
                },
            )
            .unwrap();
        case.wait(
            Duration::from_secs(15),
            || {
                fs::read_to_string(root.join("produced"))
                    .unwrap_or_default()
                    .lines()
                    .count()
                    >= 3
            },
            "provider never began",
        );
        sigkill_write(
            &root.join("source-ready.json"),
            &SigkillMarker::for_actor(&case, &receipt.identity),
        );
        loop {
            thread::park();
        }
    }
    let receipt: SigkillReceipt = sigkill_read(&root.join("owner-receipt.json"));
    assert_eq!(receipt.case, case.id);
    assert!(
        receipt.identity.record.root.is_current(),
        "the original provider must survive the host SIGKILL"
    );
    restore_orphaned_native(&host, &[]).unwrap();
    let handle = host.lock().unwrap().chats[&receipt.session].handle.clone();
    assert_eq!(handle.actor_identity(), Some(receipt.identity.clone()));
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let retained = retained_actor::retained_runs(&host);
    registry
        .lock()
        .unwrap()
        .interrupt_unproven_agent_runs_except(2, &retained)
        .unwrap();
    case.wait(
        Duration::from_secs(15),
        || {
            registry
                .lock()
                .unwrap()
                .agent_run(receipt.run)
                .unwrap()
                .state
                == State::Succeeded
        },
        "exact retained result never committed",
    );
    let run = registry.lock().unwrap().agent_run(receipt.run).unwrap();
    handle.set_visible(true);
    case.wait(
        Duration::from_secs(15),
        || {
            handle
                .snapshot(None)
                .is_some_and(|snapshot| snapshot.items.len() >= 40)
        },
        "transcript did not hydrate after host SIGKILL",
    );
    let snapshot = handle.snapshot(None).unwrap();
    assert_retained_copy_sources(&handle, &snapshot);
    let text = snapshot
        .items
        .iter()
        .filter_map(|item| match &item.content {
            chat::ItemContent::Text {
                text,
                thinking: false,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    for index in 0..40 {
        let marker = format!("FRAME_{index:03}|");
        assert_eq!(run.result.text.matches(&marker).count(), 1);
        assert_eq!(text.matches(&marker).count(), 1);
    }
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let expected_retirement = case.semantic_held.then(|| {
        let proof = host.lock().unwrap().proof().clone();
        let inspection = Handle::inspect_remote(&receipt.identity.record, proof.clone()).unwrap();
        assert_eq!(inspection.identity, receipt.identity);
        (
            proof,
            inspection.source,
            handle.launch_account_scope().unwrap().clone(),
        )
    });
    super::super::agent_execution::process(
        &host,
        Authority::default(),
        crate::agent_execution::Operation::Close(receipt.agent),
    )
    .unwrap_or_else(|error| {
        let current_proof = host.lock().unwrap().proof().clone();
        panic!(
            "{error}; {}",
            sigkill_retirement_diagnostic(&receipt.identity, &current_proof)
        );
    });
    if let Some((proof, expected_source, expected_scope)) = expected_retirement {
        let retired = Handle::retired_actor_receipt(&receipt.identity.record, &proof)
            .unwrap()
            .expect("semantic retirement must be validated before parent wait");
        assert!(retired.installed);
        assert_eq!(retired.identity, receipt.identity);
        assert_eq!(retired.proof, proof);
        assert_eq!(retired.source, expected_source);
        assert_eq!(retired.launch_account_scope, Ok(expected_scope.clone()));
        assert!(
            !receipt.identity.record.root.is_current(),
            "retirement must prove original provider absence"
        );
        let admission: SigkillAdmission = sigkill_read(&root.join("coordinator-admission.json"));
        assert_eq!(admission.case, case.id);
        assert_eq!(admission.selected_test, case.selected_test);
        assert!(admission.coordinator.is_current());
        let witness = SigkillProcess::capture(&receipt.identity.record.holder);
        assert!(
            witness.exited(),
            "exact helper execution must have exited before semantic marker"
        );
        assert_eq!(witness.parent(), Some(admission.coordinator.pid));
        assert!(
            witness.identity.is_current(),
            "helper must still be uncollected by its original parent"
        );

        // Read the actual committed saved descriptor, rather than projecting a
        // UUID or refining an issued source by hand, then use the real public
        // disk-resume entry while its old helper still has parent wait custody.
        let retirement = registry
            .lock()
            .unwrap()
            .native_actor_retirement(receipt.identity.instance)
            .unwrap()
            .unwrap();
        assert_eq!(retirement.actor(), receipt.identity.instance);
        assert_eq!(retirement.owner(), &receipt.identity.record);
        assert_eq!(retirement.scope(), Ok(&expected_scope));
        assert_eq!(retirement.source(), &retired.source);
        let canonical = retirement.source().clone();
        let confirmed = retirement
            .confirmed_session()
            .expect("completed original turn confirms a conversation");
        assert_eq!(retired.confirmed_session.as_deref(), Some(confirmed));
        assert_eq!(canonical.launch.session_id.as_deref(), Some(confirmed));
        let trace_before = fs::read(root.join("provider-trace.jsonl")).unwrap();
        assert!(trace_before.len() <= 8192);
        let ServerResponse::ChatResumePrepared { session } = process_inner(
            &host,
            17,
            ClientRequest::ChatPrepareResume {
                source: canonical.clone(),
            },
        )
        .unwrap() else {
            panic!("real held-retirement disk resume did not return a prepared pane");
        };
        assert_ne!(session.id, canonical.id);
        assert_eq!(session.launch, canonical.launch);
        assert!(
            session.proof.is_none()
                && session.terminal.is_none()
                && session.owned_codex_listener.is_none()
        );
        assert_eq!(
            session.native_continuity,
            chat::NativeContinuity::ReconnectOnly
        );
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .native_actor_retirement(receipt.identity.instance)
                .unwrap(),
            Some(retirement)
        );
        assert!(
            registry
                .lock()
                .unwrap()
                .agent(receipt.agent)
                .unwrap()
                .closed
        );
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .agent_run(receipt.run)
                .unwrap()
                .state,
            State::Succeeded
        );
        assert!(!host.lock().unwrap().chats.contains_key(&receipt.session));
        assert_eq!(
            fs::read(root.join("provider-trace.jsonl")).unwrap(),
            trace_before
        );
        assert_eq!(
            fs::read_to_string(root.join("prompts"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        assert!(witness.identity.is_current() && witness.exited());
        assert!(!receipt.identity.record.root.is_current());
        sigkill_write(
            &root.join("semantic-retirement.json"),
            &SigkillMarker::for_actor(&case, &receipt.identity),
        );
        case.wait(
            Duration::from_secs(15),
            || root.join("helper-reaped.json").exists(),
            "original parent did not collect the exact helper after semantic retirement",
        );
        let marker: SigkillMarker = sigkill_read(&root.join("helper-reaped.json"));
        marker.verify(&case, &receipt.identity);
    }
    case.wait(
        Duration::from_secs(15),
        || {
            !receipt.identity.record.holder.is_current()
                && !receipt.identity.record.root.is_current()
        },
        "explicit close did not retire exact native owners",
    );
}

/// No prompt is written in this fixture: an initialized actor still lacks a
/// provider-confirmed thread while its exact ownership proof rotates.
#[test]
fn unnamed_actor_adopts_without_forging_pane_writer_authority_before_first_provider_result() {
    let _helper_executable = retained_helper_executable();
    const MODE: &str = "CAGENTS_UNNAMED_ACTOR_PERSISTENCE_TEST";
    if std::env::var_os(MODE).is_none() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        for path in [
            "home/.claude",
            "run",
            "data",
            "config",
            "cache",
            "bin",
            "project",
        ] {
            fs::create_dir_all(root.path().join(path)).unwrap();
        }
        let test = thread::current().name().unwrap().to_owned();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env_clear()
            .env(
                "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                retained_helper_executable(),
            )
            .env(MODE, "1")
            .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
            .env("CAGENTS_HOLDER_ROOT", root.path())
            .env("HOME", root.path().join("home"))
            .env("CLAUDE_CONFIG_DIR", root.path().join("home/.claude"))
            .env("XDG_RUNTIME_DIR", root.path().join("run"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.path().join("bin").display()),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "unnamed actor fixture failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    install_provider(&root.join("bin/claude"));
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = |name| HostSessionProof {
        host_session_id: "unnamed-actor".into(),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new(name),
    };
    let first = proof("first");
    let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &first).unwrap());
    let registry_path = root.join("registry.sqlite3");
    let mut state = HostState::new(first);
    state.install_model(super::super::model::Registry::at(&registry_path).unwrap());
    let host = Arc::new(HostLock::new(state));
    let launch = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    let id = launch.id;
    start_execution(
        &host,
        0,
        launch,
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
    )
    .unwrap();
    let handle = host.lock().unwrap().chats[&id].handle.clone();
    let identity = handle.actor_identity().unwrap();
    struct Cleanup(crate::daemon::holder_registry::Record);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for process in [&self.0.root, &self.0.holder] {
                if process.is_current() {
                    unsafe {
                        libc::kill(process.pid, libc::SIGKILL);
                    }
                }
            }
        }
    }
    let _cleanup = Cleanup(identity.record.clone());
    wait_until(
        || handle.attachment_snapshot().ready,
        "actor never initialized",
    );
    assert!(
        matches!(
            host.lock().unwrap().chats[&id].session.native_continuity,
            chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
        ),
        "registered source must equal published actor capability before first input"
    );
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let authority = registry
        .lock()
        .unwrap()
        .retained_agent_authority_source(id)
        .unwrap()
        .unwrap();
    assert_eq!(
        authority,
        crate::agent_execution::HostedSession::Chat(
            host.lock().unwrap().chats[&id].session.clone()
        )
    );
    let source = host.lock().unwrap().chats[&id].session.clone();
    assert!(source.launch.session_id.is_none());
    assert!(source.has_valid_native_continuity());
    let pane = crate::model::CardId::new();
    let workspace = crate::model::WorkspaceState {
        cards: vec![crate::model::CardState {
            id: pane,
            generation: 0,
            title: "Pending conversation".into(),
            width_fraction: 1.0,
            existence: crate::model::Existence::Open,
            exposure: crate::model::Exposure::Visible,
            residency: crate::model::Residency::Hot,
            kind: crate::model::CardKind::Chat(source.clone()),
        }],
        focused: Some(pane),
        ..Default::default()
    };
    let storage_path = root.join("pane.sqlite3");
    let storage = crate::persistence::StateStore::open(
        &storage_path,
        source.proof.as_ref().unwrap().boot_id.clone(),
    )
    .unwrap();
    storage
        .save_workspace(
            &workspace,
            crate::persistence::SnapshotBase {
                workspace: 0,
                cards: &BTreeMap::new(),
            },
        )
        .unwrap();
    drop(handle);
    drop(host);
    drop(lease.take());
    let next = proof("successor");
    lease = Some(crate::daemon::HostLease::acquire(&paths, &next).unwrap());
    let mut state = HostState::new(next);
    state.install_model(super::super::model::Registry::at(&registry_path).unwrap());
    let successor = Arc::new(HostLock::new(state));
    restore_orphaned_native(&successor, &[]).unwrap();
    let handle = successor.lock().unwrap().chats[&id].handle.clone();
    assert_eq!(handle.actor_identity(), Some(identity.clone()));
    let replacement = successor.lock().unwrap().chats[&id].session.clone();
    assert!(replacement.launch.session_id.is_none());
    assert_ne!(source.proof, replacement.proof);
    // Authenticated helper adoption is not a UI writer witness. The separate
    // real client/socket contract must confirm Attach and snapshot before its
    // writer can commit this retained transition.
    assert!(chat::is_upgrade_recovery(&source, &replacement));
    assert!(
        storage
            .rebind_chat_upgrade(pane, &source, &replacement)
            .is_err()
    );
    drop(storage);
    let reopened = crate::persistence::StateStore::open(
        &storage_path,
        replacement.proof.as_ref().unwrap().boot_id.clone(),
    )
    .unwrap();
    assert_eq!(
        reopened.load_card(pane, None).unwrap().unwrap().kind,
        crate::model::CardKind::Chat(source.clone())
    );
    assert!(
        !root.join("prompts").exists(),
        "no accepted input may be replayed or synthesized"
    );
    // A later explicit first prompt names the thread without another Ready
    // edge. Public metadata must publish that helper-confirmed identity while
    // the immutable startup alias remains usable, without transcript hydration.
    let notifications = super::super::notifications::Broker::start(
        &successor,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    successor.lock().unwrap().notifications = Some(notifications.handle());
    handle
        .command(chat::Command::Prompt {
            text: "One explicit prompt after unnamed adoption".into(),
            attachments: Vec::new(),
            mode: chat::PromptMode::Send,
        })
        .unwrap();
    wait_until(
        || root.join("provider-done").exists(),
        "provider never finished its explicit first prompt",
    );
    let mut listed = None;
    wait_until(
        || {
            let (response, _, _) =
                super::super::client::process_request(&successor, 0, ClientRequest::ChatList)
                    .unwrap();
            let ServerResponse::ChatListed { sessions } = response else {
                panic!("public metadata did not return chat inventory");
            };
            listed = sessions.into_iter().find(|session| session.id == id);
            listed.as_ref().is_some_and(|session| {
                session.launch.session_id.is_some()
                    && session.launch.session_id == handle.confirmed_session_id()
            })
        },
        "unopened public inventory did not publish late confirmed identity",
    );
    let listed = listed.unwrap();
    assert_eq!(listed.launch.session_id, handle.confirmed_session_id());
    let mut expected = replacement.clone();
    expected.launch.session_id = handle.confirmed_session_id();
    assert_eq!(listed, expected);
    assert_eq!(handle.actor_identity(), Some(identity.clone()));
    {
        let state = successor.lock().unwrap();
        assert_eq!(state.chats[&id].session, replacement);
        assert!(state.chats[&id].subscribers.is_empty());
    }
    let inspection =
        Handle::inspect_remote(&identity.record, listed.proof.clone().unwrap()).unwrap();
    assert_eq!(inspection.identity, identity);
    assert_eq!(inspection.source, replacement);
    assert!(inspection.previous_sources.contains(&listed));
    assert_eq!(
        inspection
            .launch_account_scope
            .as_ref()
            .map_err(String::as_str),
        handle.launch_account_scope()
    );
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    handle.close().unwrap();
    retained_actor::finish_close(&successor, &handle, &replacement).unwrap();
    assert!(!identity.record.root.is_current() && !identity.record.holder.is_current());
    let registry = super::super::agent_execution::registry(&successor).unwrap();
    let retirement = registry
        .lock()
        .unwrap()
        .native_actor_retirement(identity.instance)
        .unwrap()
        .unwrap();
    assert_eq!(retirement.owner(), &identity.record);
    assert_eq!(
        retirement.confirmed_session(),
        handle.confirmed_session_id().as_deref()
    );
    assert_eq!(
        retirement.scope().unwrap(),
        handle.launch_account_scope().unwrap()
    );
    drop(lease);
}

/// Kill the actual host after the helper has durably retired but before the
/// host has published the registry receipt. A successor must finish that exact
/// commit using the roster and private receipt, without recreating an actor.
#[test]
fn retired_helper_receipt_completes_after_host_sigkill_before_registry_commit() {
    let _helper_executable = retained_helper_executable();
    const EXACT: &str = "host::chat::retained_actor_tests::retired_helper_receipt_completes_after_host_sigkill_before_registry_commit";
    const ROLE: &str = "CAGENTS_RETIRED_HOST_PROCESS_ROLE";
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Receipt {
        identity: chat::runtime::ActorIdentity,
        source: Session,
        launch_account_scope: crate::account::scope::LaunchAccountScope,
    }
    fn child(root: &Path, role: &str) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", EXACT, "--nocapture"])
            .env_clear()
            .env(
                "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                retained_helper_executable(),
            )
            .env(ROLE, role)
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
            );
        command
    }
    if std::env::var_os(ROLE).is_none() {
        use std::os::unix::process::ExitStatusExt;
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
            fs::create_dir_all(root.join(path)).unwrap();
        }
        install_provider(&root.join("bin/claude"));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = child(&self.0, "cleanup").output();
            }
        }
        let _cleanup = Cleanup(root.to_owned());
        struct Owner(std::process::Child);
        impl Drop for Owner {
            fn drop(&mut self) {
                if self.0.try_wait().is_ok_and(|status| status.is_none()) {
                    let _ = self.0.kill();
                }
                let _ = self.0.wait();
            }
        }
        let mut owner = Owner(
            child(root, "source")
                .stdout(fs::File::create(root.join("source.stdout")).unwrap())
                .stderr(fs::File::create(root.join("source.stderr")).unwrap())
                .spawn()
                .unwrap(),
        );
        let identity = crate::daemon::holder_registry::Process::read(owner.0.id() as i32).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !root.join("retired-before-sql.json").exists() {
            assert!(
                owner.0.try_wait().unwrap().is_none(),
                "source host failed: {}",
                fs::read_to_string(root.join("source.stderr")).unwrap_or_default()
            );
            assert!(
                Instant::now() < deadline,
                "host never reached the retirement/SQL boundary"
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(identity.is_current());
        owner.0.kill().unwrap();
        assert_eq!(owner.0.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert!(!identity.is_current());
        let output = child(root, "successor").output().unwrap();
        assert!(
            output.status.success(),
            "successor failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    let role = std::env::var(ROLE).unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("retired-host-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new(&role),
    };
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    if role == "cleanup" {
        let directory =
            crate::daemon::holder_registry::directory(crate::daemon::holder_registry::Kind::Native)
                .unwrap();
        if let Ok(records) = crate::daemon::holder_registry::discover(
            &directory,
            &proof.boot_id,
            crate::daemon::holder_registry::Kind::Native,
        ) {
            for record in records {
                // Only records from this fixture's private holder namespace.
                for process in [&record.root, &record.holder] {
                    if process.is_current() {
                        unsafe {
                            libc::kill(process.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
        return;
    }
    let path = root.join("registry.sqlite3");
    let mut state = HostState::new(proof.clone());
    state.install_model(super::super::model::Registry::at(&path).unwrap());
    let host = Arc::new(HostLock::new(state));
    if role == "source" {
        let source = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            root.join("project"),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
        let id = source.id;
        start_execution(
            &host,
            0,
            source,
            Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
        )
        .unwrap();
        let handle = host.lock().unwrap().chats[&id].handle.clone();
        let identity = handle.actor_identity().unwrap();
        struct Cleanup(crate::daemon::holder_registry::Record);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for process in [&self.0.root, &self.0.holder] {
                    if process.is_current() {
                        unsafe {
                            libc::kill(process.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
        let _cleanup = Cleanup(identity.record.clone());
        wait_until(
            || handle.attachment_snapshot().ready,
            "actor never initialized",
        );
        let source = host.lock().unwrap().chats[&id].session.clone();
        assert!(source.launch.session_id.is_none());
        assert!(handle.uncommitted_outcomes().unwrap().is_empty());
        let registry = super::super::agent_execution::registry(&host).unwrap();
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_retirement(identity.instance)
                .unwrap()
                .is_none()
        );
        handle.close().unwrap();
        // The real helper's retirement RPC persists its private receipt, checks
        // semantic ACKs and completes discovery teardown. Deliberately stop at
        // the next instruction boundary before the host's SQL receipt commit.
        handle.finish_retained_close().unwrap();
        assert!(!identity.record.holder.is_current() && !identity.record.root.is_current());
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_retirement(identity.instance)
                .unwrap()
                .is_none()
        );
        let receipt = Receipt {
            identity,
            source,
            launch_account_scope: handle
                .launch_account_scope()
                .expect("fixture captured its existing private Claude account home")
                .clone(),
        };
        let temporary = root.join("retired-before-sql.pending");
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .unwrap();
        serde_json::to_writer(&file, &receipt).unwrap();
        file.sync_all().unwrap();
        fs::rename(temporary, root.join("retired-before-sql.json")).unwrap();
        fs::File::open(&root).unwrap().sync_all().unwrap();
        loop {
            thread::park();
        }
    }
    assert_eq!(role, "successor");
    let receipt: Receipt =
        serde_json::from_slice(&fs::read(root.join("retired-before-sql.json")).unwrap()).unwrap();
    assert!(
        !receipt.identity.record.holder.is_current() && !receipt.identity.record.root.is_current()
    );
    let registry = super::super::agent_execution::registry(&host).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_retirement(receipt.identity.instance)
            .unwrap()
            .is_none()
    );
    assert!(host.lock().unwrap().chats.is_empty());
    retained_actor::reconcile_retired_actors(&host).unwrap();
    let committed = registry
        .lock()
        .unwrap()
        .native_actor_retirement(receipt.identity.instance)
        .unwrap()
        .expect("startup must commit the surviving exact retirement proof");
    assert_eq!(committed.actor(), receipt.identity.instance);
    assert_eq!(committed.owner(), &receipt.identity.record);
    assert_eq!(committed.source(), &receipt.source);
    assert_eq!(committed.confirmed_session(), None);
    assert_eq!(
        committed.scope(),
        Ok(&receipt.launch_account_scope),
        "cold reconciliation must preserve the exact captured physical account tuple",
    );
    retained_actor::reconcile_retired_actors(&host).unwrap();
    let replay = registry
        .lock()
        .unwrap()
        .native_actor_retirement(receipt.identity.instance)
        .unwrap()
        .unwrap();
    assert_eq!(replay, committed);
    assert!(
        host.lock().unwrap().chats.is_empty(),
        "receipt recovery must not create a provider handle"
    );
    assert!(
        !root.join("prompts").exists(),
        "retirement recovery must not synthesize provider input"
    );
    drop(registry);
    drop(host);
    let reopened = super::super::model::Registry::at(&path).unwrap();
    assert_eq!(
        reopened
            .native_actor_retirement(receipt.identity.instance)
            .unwrap(),
        Some(committed)
    );
}

/// Host-admission contract only: this cleanup owner is deterministic and does
/// not model an execed helper's pre-discovery lifecycle or process teardown.
#[test]
fn pending_startup_keeps_exact_cleanup_owner_until_retry_proves_teardown() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Cleanup {
        proved: AtomicBool,
        observations: AtomicUsize,
    }
    impl chat::protocol::LaunchCleanup for Cleanup {
        fn cleanup(&self) -> Result<chat::protocol::LaunchOutcome, String> {
            self.observations.fetch_add(1, Ordering::SeqCst);
            Ok(if self.proved.load(Ordering::SeqCst) {
                chat::protocol::LaunchOutcome::TeardownProved
            } else {
                chat::protocol::LaunchOutcome::Uncertain
            })
        }
    }
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let provider = root.path().join("provider");
    fs::write(&provider, "#!/bin/sh\nprintf spawned > \"$0.spawned\"\n").unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    let spawned = provider.with_extension("spawned");
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("pending-startup-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("current"),
    };
    let source = Session {
        id: TerminalId::new(),
        launch: chat::Launch {
            provider: Provider::Claude,
            recipe: crate::model::LaunchRecipe {
                program: provider.to_string_lossy().into_owned(),
                arguments: Vec::new(),
                working_directory: root.path().to_owned(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: Some(proof.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let host = Arc::new(HostLock::new(HostState::new(proof)));
    let original = Arc::new(chat::protocol::ExecutionContext::capture(root.path()).unwrap());
    let cleanup = Arc::new(Cleanup {
        proved: AtomicBool::new(false),
        observations: AtomicUsize::new(0),
    });
    original.claim_owner(cleanup.clone());
    host.lock().unwrap().native_actor_startups.insert(
        source.id,
        retained_actor::Startup {
            source: source.clone(),
            requested: source.clone(),
            context: original.clone(),
        },
    );
    for _ in 0..2 {
        let retry = Arc::new(chat::protocol::ExecutionContext::capture(root.path()).unwrap());
        assert!(
            start_with_execution(
                &host,
                0,
                source.clone(),
                None,
                false,
                None,
                Default::default(),
                Some(retry),
                None
            )
            .is_err_and(|error| error
                .message
                .contains("exact retained startup has not proved cleanup"))
        );
        let state = host.lock().unwrap();
        let pending = state.native_actor_startups.get(&source.id).unwrap();
        assert!(Arc::ptr_eq(&pending.context, &original));
        assert_eq!(pending.source, source);
        assert!(state.chat_spawning.is_empty());
        assert!(state.chats.is_empty());
        assert!(
            !spawned.exists(),
            "retry may not start a provider before original cleanup proof"
        );
    }
    assert_eq!(cleanup.observations.load(Ordering::SeqCst), 2);
    cleanup.proved.store(true, Ordering::SeqCst);
    assert!(retained_actor::reconcile_startup(&host, &source).unwrap());
    assert!(host.lock().unwrap().native_actor_startups.is_empty());
    assert_eq!(cleanup.observations.load(Ordering::SeqCst), 3);
    assert_eq!(
        original.outcome(),
        chat::protocol::LaunchOutcome::TeardownProved
    );
    assert!(!spawned.exists());
}

#[test]
fn unattached_claude_cleanup_authenticates_old_alias_and_closes_original_pair() {
    let _helper_executable = retained_helper_executable();
    const MODE: &str = "CAGENTS_UNATTACHED_CLAUDE_CLOSE_TEST";
    if std::env::var_os(MODE).is_none() {
        for mode in ["unpaired", "paired", "removed-home", "replaced-home"] {
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
                fs::create_dir_all(root.join(path)).unwrap();
            }
            let test = thread::current().name().unwrap().to_owned();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test, "--nocapture"])
                .env_clear()
                .env(
                    "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                    retained_helper_executable(),
                )
                .env(MODE, mode)
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
                "unattached cleanup mode={mode} failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    let mode = std::env::var(MODE).unwrap();
    let paired = mode == "paired";
    install_provider(&root.join("bin/claude"));
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = |incarnation| HostSessionProof {
        host_session_id: HostSessionId::new("unattached-close-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new(incarnation),
    };
    let first = proof("first");
    let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &first).unwrap());
    let registry_path = root.join("registry.sqlite3");
    let mut state = HostState::new(first.clone());
    state.enable_holder_recovery();
    state.install_model(super::super::model::Registry::at(&registry_path).unwrap());
    let host = Arc::new(HostLock::new(state));
    let launch = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    let id = launch.id;
    start_execution(
        &host,
        0,
        launch,
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
    )
    .unwrap();
    let handle = host.lock().unwrap().chats[&id].handle.clone();
    let identity = handle.actor_identity().unwrap();
    struct Cleanup(Vec<crate::daemon::holder_registry::Record>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for owner in &self.0 {
                for process in [&owner.root, &owner.holder] {
                    if process.is_current() {
                        unsafe {
                            libc::kill(process.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
    }
    let mut cleanup = Cleanup(vec![identity.record.clone()]);
    wait_until(
        || handle.attachment_snapshot().ready,
        "actor never initialized",
    );
    // This alias predates pair publication and the successor daemon's proof.
    let requested = host.lock().unwrap().chats[&id].session.clone();
    let original_scope = handle.launch_account_scope().unwrap().clone();
    // Obtain a real provider-confirmed identity from one explicit offline turn.
    // Neither the issued allocation nor an input correlation UUID proves it.
    handle
        .command(chat::Command::Prompt {
            text: "One explicit turn before cleanup and resume qualification".into(),
            attachments: Vec::new(),
            mode: chat::PromptMode::Send,
        })
        .unwrap();
    wait_until(
        || root.join("provider-done").exists() && handle.confirmed_session_id().is_some(),
        "fixture provider never confirmed its completed conversation",
    );
    let mut confirmed_source = None;
    wait_until(
        || {
            let (response, _, _) =
                super::super::client::process_request(&host, 17, ClientRequest::ChatList).unwrap();
            let ServerResponse::ChatListed { sessions } = response else {
                panic!("public metadata did not return chat inventory");
            };
            confirmed_source = sessions.into_iter().find(|session| session.id == id);
            confirmed_source.as_ref().is_some_and(|session| {
                session.launch.session_id.is_some()
                    && session.launch.session_id == handle.confirmed_session_id()
            })
        },
        "public inventory never published its provider-confirmed descriptor",
    );
    let confirmed_source = confirmed_source.unwrap();
    assert_eq!(
        confirmed_source.launch.session_id,
        handle.confirmed_session_id()
    );
    let mut expected = requested.clone();
    expected.launch.session_id = handle.confirmed_session_id();
    assert_eq!(confirmed_source, expected);
    assert_eq!(host.lock().unwrap().chats[&id].session, requested);
    assert_eq!(handle.actor_identity(), Some(identity.clone()));
    let inspection = Handle::inspect_remote(&identity.record, first.clone()).unwrap();
    assert_eq!(inspection.identity, identity);
    assert_eq!(inspection.source, requested);
    assert!(inspection.previous_sources.contains(&confirmed_source));
    assert_eq!(inspection.launch_account_scope, Ok(original_scope.clone()));
    let ServerResponse::ChatResumePrepared { session: live } = process_inner(
        &host,
        17,
        ClientRequest::ChatPrepareResume {
            source: confirmed_source.clone(),
        },
    )
    .unwrap() else {
        panic!("a live owner must route to its existing pane");
    };
    assert_eq!(
        live, confirmed_source,
        "live-owner routing must not allocate a new pane or launch"
    );
    assert!(identity.record.holder.is_current() && identity.record.root.is_current());
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let pair = if paired {
        let recipe = crate::model::LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: root.join("project"),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        host.lock()
            .unwrap()
            .chats
            .get_mut(&id)
            .unwrap()
            .console_recipe = Some(recipe.clone());
        let terminal =
            crate::terminal_ipc::TerminalDescriptor::relaunchable(id, recipe.clone(), None).session;
        let (descriptor, console) = super::super::state::spawn_terminal_view_owned(
            &host,
            0,
            terminal,
            Default::default(),
            None,
            true,
        )
        .unwrap();
        let owner = console.holder_identity().unwrap().clone();
        cleanup.0.push(owner.clone());
        let mut source = confirmed_source.clone();
        source.terminal = Some(descriptor.session);
        handle
            .update_native_source(source.clone(), Some(recipe), Some(owner.clone()))
            .unwrap();
        {
            let mut state = host.lock().unwrap();
            let record = state.chats.get_mut(&id).unwrap();
            record.session = source.clone();
            record.paired_terminal = Some(console.clone());
        }
        retained_actor::register_ownership(&host, &handle, &source).unwrap();
        console.preserve_for_restart().unwrap();
        Some(owner)
    } else {
        None
    };
    drop(handle);
    drop(host);
    drop(lease.take());
    if mode == "removed-home" || mode == "replaced-home" {
        fs::rename(
            root.join("home/.claude"),
            root.join("home/.claude-original"),
        )
        .unwrap();
        if mode == "replaced-home" {
            fs::create_dir(root.join("home/.claude")).unwrap();
        }
        assert!(original_scope.revalidate().is_err());
    }
    assert!(identity.record.holder.is_current() && identity.record.root.is_current());
    let next = proof("successor");
    lease = Some(crate::daemon::HostLease::acquire(&paths, &next).unwrap());
    let mut state = HostState::new(next.clone());
    state.enable_holder_recovery();
    state.install_model(super::super::model::Registry::at(&registry_path).unwrap());
    let successor = Arc::new(HostLock::new(state));
    let _notifications = super::super::notifications::Broker::start(
        &successor,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    successor.lock().unwrap().notifications = Some(_notifications.handle());
    if paired {
        super::super::state::restore_orphaned_holders(&successor).unwrap();
    }
    assert!(successor.lock().unwrap().chats.is_empty());
    if mode == "removed-home" || mode == "replaced-home" {
        assert!(
            retained_actor::restore(&successor, 0, &requested, &next, identity.record.clone())
                .is_err(),
            "normal attachment must retain physical-account admission"
        );
        assert!(successor.lock().unwrap().chats.is_empty());
        assert!(identity.record.holder.is_current() && identity.record.root.is_current());
    }
    let mut changed_account = requested.clone();
    changed_account.launch.recipe.environment.insert(
        "CLAUDE_CONFIG_DIR".into(),
        root.join("other-account").to_string_lossy().into_owned(),
    );
    assert!(super::native_cleanup::close_retained(&successor, &changed_account).is_err());
    assert!(identity.record.holder.is_current() && identity.record.root.is_current());
    assert!(successor.lock().unwrap().chat_spawning.is_empty());
    if paired {
        let fault = rusqlite::Connection::open(&registry_path).unwrap();
        fault.execute_batch("CREATE TRIGGER reject_unattached_retirement BEFORE INSERT ON native_actor_retirements BEGIN SELECT RAISE(ABORT, 'fixture unattached retirement commit failure'); END;").unwrap();
        assert!(
            super::native_cleanup::close_retained(&successor, &requested)
                .is_err_and(|error| error.contains("fixture unattached retirement commit failure"))
        );
        assert!(!identity.record.holder.is_current() && !identity.record.root.is_current());
        let state = successor.lock().unwrap();
        assert!(
            state.chats.contains_key(&id),
            "failed durable retirement must retain exact cleanup handle"
        );
        assert!(state.terminals[&id].handle.is_completed());
        assert!(state.chats[&id].handle.is_close_only());
        assert!(state.chat_spawning.is_empty());
        let closed_source = state.chats[&id].session.clone();
        drop(state);
        assert!(
            process_inner(
                &successor,
                17,
                ClientRequest::ChatAttach {
                    session: closed_source
                }
            )
            .is_err(),
            "a cleanup-only owner must not be published as an attached conversation"
        );
        fault
            .execute_batch("DROP TRIGGER reject_unattached_retirement;")
            .unwrap();
        drop(fault);
    }
    super::native_cleanup::close_retained(&successor, &requested).unwrap();
    assert!(!identity.record.holder.is_current() && !identity.record.root.is_current());
    if let Some(pair) = pair {
        assert!(!pair.holder.is_current() && !pair.root.is_current());
    }
    let state = successor.lock().unwrap();
    assert!(!state.chats.contains_key(&id));
    assert!(!state.terminals.contains_key(&id));
    assert!(state.chat_spawning.is_empty());
    drop(state);
    let registry = super::super::agent_execution::registry(&successor).unwrap();
    let retirement = registry
        .lock()
        .unwrap()
        .native_actor_retirement(identity.instance)
        .unwrap()
        .unwrap();
    assert_eq!(retirement.owner(), &identity.record);
    assert_eq!(
        retirement.scope(),
        Ok(&original_scope),
        "cleanup retains the originally captured account tuple"
    );
    let canonical = retirement.source().clone();
    let confirmed = retirement
        .confirmed_session()
        .expect("actual completed fixture turn must confirm a conversation");
    assert_eq!(
        canonical.launch.session_id.as_deref(),
        Some(confirmed),
        "resume must use the actual canonical retirement descriptor"
    );
    assert!(
        process_inner(
            &successor,
            17,
            ClientRequest::ChatPrepareResume {
                source: requested.clone()
            }
        )
        .is_err(),
        "an actually saved unnamed alias cannot acquire a fabricated confirmed identity"
    );
    let mut wrong_recipe = canonical.clone();
    wrong_recipe
        .launch
        .recipe
        .arguments
        .push("--not-the-issued-recipe".into());
    assert!(
        process_inner(
            &successor,
            17,
            ClientRequest::ChatPrepareResume {
                source: wrong_recipe
            }
        )
        .is_err()
    );
    let mut wrong_protocol = canonical.clone();
    wrong_protocol.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 1 };
    assert!(
        process_inner(
            &successor,
            17,
            ClientRequest::ChatPrepareResume {
                source: wrong_protocol
            }
        )
        .is_err()
    );
    let prepared = process_inner(
        &successor,
        17,
        ClientRequest::ChatPrepareResume {
            source: canonical.clone(),
        },
    );
    if mode == "removed-home" || mode == "replaced-home" {
        assert!(
            prepared.is_err(),
            "close-only authority must not authorize resume through a changed account home"
        );
    } else {
        let ServerResponse::ChatResumePrepared { session } = prepared.unwrap() else {
            panic!("qualified retirement must prepare the exact saved conversation");
        };
        assert_ne!(session.id, canonical.id);
        assert_eq!(session.launch, canonical.launch);
        assert_eq!(session.launch.session_id.as_deref(), Some(confirmed));
        assert_eq!(session.proof, None);
        assert_eq!(session.terminal, None);
        assert_eq!(session.owned_codex_listener, None);
        assert_eq!(
            session.native_continuity,
            chat::NativeContinuity::ReconnectOnly
        );
        let registry = registry.lock().unwrap();
        let predecessor = registry
            .session_lineage(canonical.id)
            .unwrap()
            .unwrap_or(crate::agent_execution::Lineage(canonical.id));
        assert_eq!(
            registry.session_lineage(session.id).unwrap(),
            Some(predecessor)
        );
    }
    assert!(
        successor.lock().unwrap().chats.is_empty(),
        "resume preparation must not launch a provider"
    );
    assert!(!identity.record.holder.is_current() && !identity.record.root.is_current());
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1,
        "cleanup and resume preparation must never replay the one explicit fixture prompt"
    );
    drop(lease);
}

#[test]
fn unattached_claude_cleanup_refuses_unsupported_protocol_before_reservation() {
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("unsupported-claude-close"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("current"),
    };
    let source = Session {
        id: TerminalId::new(),
        launch: chat::Launch {
            provider: Provider::Claude,
            recipe: crate::model::LaunchRecipe {
                program: "/not-a-real-provider".into(),
                arguments: Vec::new(),
                working_directory: "/var/tmp".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: Some(proof.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: chat::NativeContinuity::RetainedClaudeActor { protocol: 1 },
    };
    let host = Arc::new(HostLock::new(HostState::new(proof)));
    assert!(
        super::native_cleanup::close_retained(&host, &source)
            .is_err_and(|error| error.contains("protocol is unsupported"))
    );
    let state = host.lock().unwrap();
    assert!(state.chat_spawning.is_empty());
    assert!(state.chats.is_empty());
    assert!(state.native_closing.is_empty());
}
