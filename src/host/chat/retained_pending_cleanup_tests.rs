//! Actual constructor-to-HOST publication faults retain cleanup authority.
//! Lease replacement exercises HOST loss; this is not a SIGKILL/native UI test.
use super::*;
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

#[test]
fn pending_publication_failure_closed_agent_cold_cleanup() {
    cleanup_after_publication(false, false);
}

#[test]
fn installed_closed_agent_cold_explicit_cleanup() {
    cleanup_after_publication(true, false);
}

#[test]
fn installed_helper_without_census_refuses_adoption_before_epoch_change() {
    cleanup_after_publication(false, true);
}

fn cleanup_after_publication(installed: bool, unknown_installed: bool) {
    const CHILD: &str = "CAGENTS_PENDING_CLEANUP_TEST";
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
            fs::create_dir_all(root.join(path)).unwrap();
        }
        let exact = thread::current().name().unwrap().to_owned();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &exact, "--nocapture"])
            .env_clear()
            .env(CHILD, "1")
            .env(
                "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                retained_actor_tests::retained_helper_executable(),
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
    fn wait(mut condition: impl FnMut() -> bool, reason: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(Instant::now() < deadline, "{reason}");
            let ready = condition();
            // Local reads and typed calls are not an OS watchdog, but a late
            // observation cannot reset this fixture's acceptance cutoff.
            assert!(Instant::now() < deadline, "{reason}");
            if ready {
                break;
            }
            thread::sleep(
                Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    fn host(
        root: &Path,
        proof: HostSessionProof,
    ) -> (SharedHost, super::super::notifications::Broker) {
        let mut state = HostState::new(proof);
        state.install_model(
            super::super::model::Registry::at(&root.join("registry.sqlite3")).unwrap(),
        );
        let host = Arc::new(HostLock::new(state));
        let notifications = super::super::notifications::Broker::start(
            &host,
            crate::notifications::Application::Cagents,
        )
        .unwrap();
        host.lock().unwrap().notifications = Some(notifications.handle());
        (host, notifications)
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    if installed || unknown_installed {
        retained_actor_tests::install_provider(&root.join("bin/claude"));
    } else {
        install_boot_permission_provider(&root.join("bin/claude"));
    }
    let proof = |name| HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("pending-cleanup-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: crate::model::HostIncarnation::new(name),
    };
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let first = proof("pending-owner");
    let lease = crate::daemon::HostLease::acquire(&paths, &first).unwrap();
    let (original, notifications) = host(&root, first.clone());
    let mut source = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    source.proof = Some(first.clone());
    let id = source.id;
    let registry = super::super::agent_execution::registry(&original).unwrap();
    let accepted = {
        use crate::agent_execution::{Destination, Origin, Spec, Submit, Surface, Target};
        let mut registry = registry.lock().unwrap();
        registry
            .apply(&crate::workspace_model::ModelEdit::OpenProject {
                repository: root.join("project"),
                name: "Pending cleanup fixture".into(),
                kind: crate::model::ProjectKind::Folder,
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
                    prompt: "never sent".into(),
                    idempotency_key: "cleanup-only".into(),
                },
                1,
            )
            .unwrap();
        registry
            .bind_agent(
                accepted.agent.id,
                None,
                crate::agent_execution::HostedSession::Chat(source.clone()),
            )
            .unwrap();
        accepted
    };
    registry
        .lock()
        .unwrap()
        .fresh_agent_authority(&crate::agent_execution::HostedSession::Chat(source.clone()))
        .unwrap();
    let context =
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap());
    // The real HOST runtime constructor returns its execed startup owner before
    // installation. Deliberately stop here: no commit request was applied.
    let (mut spec, adapter) = chat::protocol::create(&source.launch).unwrap();
    spec.execution = Some(context.clone());
    let prepared = adapter.prepared_launch_identity().unwrap();
    let environment = spec.environment.clone();
    let pending =
        Handle::start_remote_owned(spec, source.clone(), prepared, environment, |_| {}).unwrap();
    let identity = pending.actor_identity().unwrap();
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
    let raw = Handle::inspect_remote(&identity.record, first.clone()).unwrap();
    assert!(!raw.state.installed);
    assert_eq!(raw.source, source);
    if unknown_installed {
        // Deliberately install the actual helper without publishing any census
        // row. Recovery must not backfill authority from an installed process.
        pending.commit_native_installation().unwrap();
        assert!(
            Handle::inspect_remote(&identity.record, first.clone())
                .unwrap()
                .state
                .installed
        );
        let (candidate, _notifications) = host(&root, first.clone());
        let refusal = super::native_recovery::restore(&candidate, 0, &source).unwrap_err();
        assert!(!refusal.is_empty());
        assert!(candidate.lock().unwrap().chats.is_empty());
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_ownership(identity.instance)
                .unwrap()
                .is_none()
        );
        let unchanged = Handle::inspect_remote(&identity.record, first.clone()).unwrap();
        assert_eq!(unchanged.identity, identity);
        assert_eq!(unchanged.source, source);
        // The same live lease is intentional: a successful benign command from
        // the original handle proves recovery did not supersede its epoch.
        pending.command(chat::Command::Refresh).unwrap();
        assert!(!root.join("prompts").exists());
        pending.close().unwrap();
        pending.finish_retained_close().unwrap();
        wait(
            || !identity.record.root.is_current() && !identity.record.holder.is_current(),
            "original no-census owner did not retire its exact helper/provider",
        );
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_ownership(identity.instance)
                .unwrap()
                .is_none()
        );
        context.installed();
        return;
    }
    if !installed {
        wait(
            || pending.attachment_snapshot().ready,
            "private provider initialize did not reach Ready baseline",
        );
        fs::write(root.join("emit-boot-permission"), "emit").unwrap();
        wait(
            || {
                Handle::inspect_remote(&identity.record, first.clone()).is_ok_and(|inspection| {
                    inspection.state.outbox_latest > inspection.state.outbox_ack
                })
            },
            "actual boot permission request produced no retained semantic evidence",
        );
    }
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_ownership(identity.instance)
            .unwrap()
            .is_none()
    );
    original.lock().unwrap().native_actor_startups.insert(
        id,
        retained_actor::Startup {
            source: source.clone(),
            requested: source.clone(),
            context: context.clone(),
            failed_close: None,
        },
    );
    drop(lease);
    let second = proof("publication-attempt");
    let lease = crate::daemon::HostLease::acquire(&paths, &second).unwrap();
    let (failed, failed_notifications) = host(&root, second.clone());
    let fault = rusqlite::Connection::open(root.join("registry.sqlite3")).unwrap();
    if !installed {
        fault.execute_batch("CREATE TRIGGER reject_pending_publication BEFORE UPDATE ON agent_authorities BEGIN SELECT RAISE(ABORT, 'fixture pending-installed-before-registry failure'); END;").unwrap();
        let refusal = super::native_recovery::restore(&failed, 0, &source).unwrap_err();
        assert!(
            refusal.contains("fixture pending-installed-before-registry failure"),
            "{refusal}"
        );
        assert!(!failed.lock().unwrap().chats.contains_key(&id));
        fault
            .execute_batch("DROP TRIGGER reject_pending_publication;")
            .unwrap();
    } else {
        super::native_recovery::restore(&failed, 0, &source).unwrap();
        assert!(failed.lock().unwrap().chats.contains_key(&id));
    }
    drop(fault);
    let after_ack = Handle::inspect_remote(&identity.record, second.clone()).unwrap();
    assert!(
        after_ack.state.installed,
        "the cutpoint must follow real helper installation"
    );
    assert_eq!(after_ack.identity, identity);
    assert_eq!(after_ack.source.proof, Some(second.clone()));
    assert!(after_ack.previous_sources.contains(&source));
    let ownership = registry
        .lock()
        .unwrap()
        .native_actor_ownership(identity.instance)
        .unwrap()
        .unwrap();
    assert_eq!(ownership.is_pending(), !installed);
    assert_eq!(ownership.actor(), identity.instance);
    assert_eq!(ownership.owner(), &identity.record);
    assert_eq!(
        ownership.scope(),
        raw.launch_account_scope.as_ref().map_err(String::as_str)
    );
    if !installed {
        assert_eq!(
            ownership.source(),
            &source,
            "the durable intent preserves the authenticated raw constructor descriptor"
        );
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .retained_agent_authority_source(id)
                .unwrap(),
            Some(crate::agent_execution::HostedSession::Chat(source.clone()))
        );
    } else {
        assert_eq!(ownership.source(), &after_ack.source);
    }
    // This accepted fixture request never reached the provider. Settle it through
    // the typed run API before closing the actual bound Agent; no SQL state is forged.
    {
        use crate::agent_execution::{Origin, ResultSummary, State, Update};
        let mut registry = registry.lock().unwrap();
        registry
            .update_agent_run(
                accepted.run.id,
                accepted.run.revision,
                Update::Finished {
                    state: State::Interrupted,
                    result: ResultSummary {
                        diagnostic: Some("Fixture request was never sent to the provider".into()),
                        ..Default::default()
                    },
                },
                2,
            )
            .unwrap();
        assert!(
            registry
                .close_agent(&Origin::User, accepted.agent.id)
                .unwrap()
                .closed
        );
    }
    assert!(!root.join("prompts").exists());
    // Dispose old owners without calling stale-epoch cleanup. The provider must
    // survive these drops so only the new HOST's real close can satisfy the test.
    context.installed();
    drop(pending);
    drop(context);
    drop(notifications);
    drop(original);
    let failed_weak = Arc::downgrade(&failed);
    drop(failed_notifications);
    drop(failed);
    drop(lease);
    wait(
        || failed_weak.strong_count() == 0,
        "publication HOST did not detach",
    );
    assert!(identity.record.root.is_current() && identity.record.holder.is_current());
    // Replace only this fixture's private physical home. Its captured directory
    // remains on disk, proving cleanup uses immutable captured evidence rather
    // than requiring that path to still identify the original inode.
    if !installed {
        fs::rename(
            root.join("home/.claude"),
            root.join("home/.claude-original"),
        )
        .unwrap();
        fs::create_dir(root.join("home/.claude")).unwrap();
    }
    let third = proof("cleanup-successor");
    let _lease = crate::daemon::HostLease::acquire(&paths, &third).unwrap();
    let (successor, _notifications) = host(&root, third.clone());
    let refusal = super::native_recovery::restore(&successor, 0, &after_ack.source).unwrap_err();
    assert!(!refusal.is_empty());
    assert!(successor.lock().unwrap().chats.is_empty());
    if installed {
        retained_actor::close_unattached(
            &successor,
            &after_ack.source,
            Some(identity.record.clone()),
        )
        .unwrap();
    } else {
        // Fault only notification persistence. Registry must commit this actual
        // non-run Waiting evidence before notification failure prevents helper ACK.
        let attention_path = crate::instance::project_dirs()
            .unwrap()
            .data_dir()
            .join("agent-attention.sqlite3");
        wait(
            || {
                attention_path.exists() && rusqlite::Connection::open(&attention_path).is_ok_and(|db|
            db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='seen')",
                [], |row| row.get::<_, bool>(0)).unwrap_or(false))
            },
            "notification journal did not initialize",
        );
        let attention_fault = rusqlite::Connection::open(&attention_path).unwrap();
        attention_fault.execute_batch("CREATE TRIGGER reject_pending_attention BEFORE INSERT ON seen BEGIN SELECT RAISE(ABORT, 'fixture Pending attention commit failure'); END;").unwrap();
        // An actor-local failure must leave startup available for other actors.
        retained_actor::reconcile_retired_actors(&successor).unwrap();
        let registry = super::super::agent_execution::registry(&successor).unwrap();
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .native_actor_ownership(identity.instance)
                .unwrap(),
            Some(ownership.clone()),
            "startup must retain the exact Pending intent"
        );
        // Retry the already attached close-only owner while the trigger still
        // fails, proving the expected notification fault caused the retention.
        let refusal = retained_actor::close_unattached(
            &successor,
            &after_ack.source,
            Some(identity.record.clone()),
        )
        .unwrap_err();
        assert!(
            refusal.contains("fixture Pending attention commit failure"),
            "{refusal}"
        );
        assert!(
            identity.record.holder.is_current(),
            "unacknowledged helper was retired"
        );
        let held = Handle::inspect_remote(&identity.record, third.clone()).unwrap();
        assert!(
            held.state.outbox_latest > held.state.outbox_ack,
            "notification failure must leave actual semantic evidence unacknowledged"
        );
        let registry = super::super::agent_execution::registry(&successor).unwrap();
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_cursor_for_test(identity.instance)
                .unwrap()
                .is_some_and(|cursor| cursor > 0),
            "non-run semantic evidence must commit before helper ACK"
        );
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_retirement(identity.instance)
                .unwrap()
                .is_none()
        );
        attention_fault
            .execute_batch("DROP TRIGGER reject_pending_attention;")
            .unwrap();
        // The broker retains its failed page for retry. Observe the actual
        // helper ACK while it is still alive: production only ACKs after both
        // cursor and notification commits. Explicit close retires seen rows, so
        // a post-close SQL query would race that intended journal cleanup.
        wait(
            || {
                Handle::inspect_remote(&identity.record, third.clone()).is_ok_and(|inspection| {
                    inspection.state.outbox_latest > 0
                        && inspection.state.outbox_ack == inspection.state.outbox_latest
                })
            },
            "notification retry never committed and ACKed the actual semantic page",
        );
        // The failed close intentionally retained its exact close-only HOST
        // record. Retry that owning entry rather than restarting a provider.
        retained_actor::close_unattached(
            &successor,
            &after_ack.source,
            Some(identity.record.clone()),
        )
        .unwrap();
    }
    wait(
        || !identity.record.root.is_current() && !identity.record.holder.is_current(),
        "cold cleanup did not retire the exact fixture provider/helper",
    );
    let registry = super::super::agent_execution::registry(&successor).unwrap();
    let retirement = registry
        .lock()
        .unwrap()
        .native_actor_retirement(identity.instance)
        .unwrap()
        .unwrap();
    assert_eq!(
        retirement.is_pending(),
        !installed,
        "cleanup cannot promote Pending into installed or disk-resume authority"
    );
    assert_eq!(retirement.actor(), identity.instance);
    assert_eq!(retirement.owner(), &identity.record);
    assert_eq!(retirement.scope(), ownership.scope());
    assert_eq!(retirement.confirmed_session(), None);
    assert!(
        registry
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .closed
    );
    if !installed {
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .retained_agent_authority_source(id)
                .unwrap(),
            Some(crate::agent_execution::HostedSession::Chat(source))
        );
    }
    assert!(successor.lock().unwrap().chats.is_empty());
    assert!(
        !root.join("prompts").exists(),
        "cleanup must never grant or replay provider input"
    );
    retained_actor::reconcile_retired_actors(&successor).unwrap();
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .native_actor_retirement(identity.instance)
            .unwrap(),
        Some(retirement)
    );
}

/// An offline real protocol peer emits one gated permission request after the
/// adapter's Ready baseline, without any user prompt or fabricated HOST update.
fn install_boot_permission_provider(path: &Path) {
    fs::write(path, r#"#!/usr/bin/python3
import json, pathlib, select, sys
root=pathlib.Path(__file__).resolve().parent.parent
initialized=False
sent=False
def send(value): print(json.dumps(value),flush=True)
while True:
    if initialized and not sent and (root/'emit-boot-permission').exists():
        send({'type':'control_request','request_id':'pending-boot-permission','request':{'subtype':'can_use_tool','tool_name':'Read','tool_use_id':'pending-boot-read','input':{'file_path':'/fixture/read-only'}}})
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
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
