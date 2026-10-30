//! Real HOST entry paths consume authenticated execution-exit retirement.
//! Parent wait collection is separately asserted; it is never inferred from a
//! successful Close or used to manufacture a provider-confirmed identity.
//! Cold replay here runs after original-parent collection. The separate actual
//! host-SIGKILL fixture owns unreaped-helper dispatch coverage; this fixture
//! does not substitute for that execution-exit/parent-custody boundary.
use super::*;
use std::{fs, path::Path};

#[test]
fn retired_execution_routes_cold_close_reconcile_and_disk_resume_without_live_adoption() {
    const MODE: &str = "CAGENTS_RETAINED_EXECUTION_EXIT_TEST";
    if std::env::var_os(MODE).is_none() {
        for mode in ["cold-close", "reconcile"] {
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
                .env(MODE, mode)
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
                "mode={mode}\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    }
    fn wait(mut predicate: impl FnMut() -> bool, reason: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !predicate() {
            assert!(Instant::now() < deadline, "{reason}");
            thread::sleep(Duration::from_millis(10));
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
    retained_actor_tests::install_provider(&root.join("bin/claude"));
    let proof = |name| HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("execution-exit-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: crate::model::HostIncarnation::new(name),
    };
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let first = proof("first");
    let lease = crate::daemon::HostLease::acquire(&paths, &first).unwrap();
    let (original, notifications) = host(&root, first.clone());
    let source = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    let id = source.id;
    start_execution(
        &original,
        0,
        source,
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
    )
    .unwrap();
    let handle = original.lock().unwrap().chats[&id].handle.clone();
    let identity = handle.actor_identity().unwrap();
    // Fixture teardown retains the exact issued process identities; it never
    // signals a remembered PID after its incarnation is no longer current.
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
    wait(
        || handle.attachment_snapshot().ready,
        "real actor did not initialize",
    );
    assert!(matches!(
        Handle::inspect_retained_retirement(&identity.record, &first).unwrap(),
        chat::runtime::RetirementInspection::Running
    ));
    handle
        .command(chat::Command::Prompt {
            text: "One explicit confirmation before execution-exit cleanup".into(),
            attachments: vec![],
            mode: chat::PromptMode::Send,
        })
        .unwrap();
    // The hosted Record retains its issued startup descriptor. Public list
    // and attachment responses project the current provider-confirmed ID.
    // One acceptance cutoff covers every poll and both public calls; it does
    // not preempt synchronous handler I/O, which the outer wrapper bounds.
    let confirmation_deadline = Instant::now() + Duration::from_secs(15);
    let canonical = loop {
        assert!(
            Instant::now() < confirmation_deadline,
            "HOST never published actual provider confirmation"
        );
        if root.join("provider-done").exists()
            && let Some(confirmed) = handle.confirmed_session_id()
        {
            let ServerResponse::ChatListed { sessions } =
                process(&original, 0, ClientRequest::ChatList).unwrap()
            else {
                panic!("public chat list did not return its inventory");
            };
            assert!(
                Instant::now() < confirmation_deadline,
                "public chat list exceeded the confirmation cutoff"
            );
            let listed = sessions.into_iter().find(|session| session.id == id);
            if let Some(listed) = listed
                && listed.launch.session_id.as_deref() == Some(confirmed.as_str())
            {
                let ServerResponse::ChatAttached { session, snapshot } = process(
                    &original,
                    0,
                    ClientRequest::ChatAttach {
                        session: listed.clone(),
                    },
                )
                .unwrap() else {
                    panic!("public chat attachment did not return its snapshot");
                };
                assert!(
                    Instant::now() < confirmation_deadline,
                    "public attachment exceeded the confirmation cutoff"
                );
                assert_eq!(session.id, id);
                assert_eq!(session.launch, listed.launch);
                assert_eq!(snapshot.session_id.as_deref(), Some(confirmed.as_str()));
                assert_eq!(handle.actor_identity(), Some(identity.clone()));
                break session;
            }
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(canonical.launch.session_id, handle.confirmed_session_id());
    assert!(canonical.launch.session_id.is_some());
    let ServerResponse::ChatResumePrepared { session: live } = process_inner(
        &original,
        0,
        ClientRequest::ChatPrepareResume {
            source: canonical.clone(),
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(
        live, canonical,
        "a running actor must keep its existing pane"
    );
    let fault = rusqlite::Connection::open(root.join("registry.sqlite3")).unwrap();
    fault.execute_batch("CREATE TRIGGER reject_execution_exit_retirement BEFORE INSERT ON native_actor_retirements BEGIN SELECT RAISE(ABORT, 'execution-exit retirement SQL boundary'); END;").unwrap();
    let error = process(&original, 0, ClientRequest::ChatClose { chat_id: id }).unwrap_err();
    assert!(
        error
            .message
            .contains("execution-exit retirement SQL boundary"),
        "{}",
        error.message
    );
    let verified = match Handle::inspect_retained_retirement(&identity.record, &first).unwrap() {
        chat::runtime::RetirementInspection::Verified(receipt) => receipt,
        _ => panic!("actual Close did not publish verified semantic retirement"),
    };
    let canonical = verified.receipt().source.clone();
    assert_eq!(
        verified.receipt().confirmed_session,
        canonical.launch.session_id
    );
    assert!(
        !identity.record.root.is_current(),
        "provider absence is required independently of helper exit"
    );
    assert!(
        super::super::agent_execution::registry(&original)
            .unwrap()
            .lock()
            .unwrap()
            .native_actor_retirement(identity.instance)
            .unwrap()
            .is_none()
    );
    drop(verified);
    // This is a second, independent custody oracle. The execution-exit capsule
    // above does not stand in for collection of the original Child by its parent.
    wait(
        || !identity.record.holder.is_current(),
        "the exact parent did not collect its helper child",
    );
    drop(handle);
    drop(notifications);
    drop(original);
    drop(lease);
    fault
        .execute_batch("DROP TRIGGER reject_execution_exit_retirement;")
        .unwrap();
    drop(fault);
    let next = proof("successor");
    let _lease = crate::daemon::HostLease::acquire(&paths, &next).unwrap();
    let (successor, _notifications) = host(&root, next.clone());
    assert!(successor.lock().unwrap().chats.is_empty());
    // One unproved exited actor must not block unrelated HOST admission.
    let healthy = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    let healthy_id = healthy.id;
    start_execution(
        &successor,
        0,
        healthy,
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
    )
    .unwrap();
    let healthy_handle = successor.lock().unwrap().chats[&healthy_id].handle.clone();
    let healthy_identity = healthy_handle.actor_identity().unwrap();
    let _healthy_cleanup = Cleanup(healthy_identity.record.clone());
    wait(
        || healthy_handle.attachment_snapshot().ready,
        "unrelated actor did not initialize",
    );
    let private =
        crate::daemon::holder_registry::directory(crate::daemon::holder_registry::Kind::Native)
            .unwrap();
    let retired = private.join(format!(
        "{}-{}-{}.retired",
        id, identity.record.holder.pid, identity.record.holder.start
    ));
    let held = retired.with_extension("fixture-held");
    fs::rename(&retired, &held).unwrap();
    assert!(matches!(
        Handle::inspect_retained_retirement(&identity.record, &next).unwrap(),
        chat::runtime::RetirementInspection::ExitedUnproved
    ));
    retained_actor::reconcile_retired_actors(&successor).unwrap();
    assert!(
        super::super::agent_execution::registry(&successor)
            .unwrap()
            .lock()
            .unwrap()
            .native_actor_retirement(identity.instance)
            .unwrap()
            .is_none(),
        "unproved actor must remain retained without blocking daemon reconciliation"
    );
    assert!(super::native_cleanup::close_retained(&successor, &canonical).is_err());
    assert!(
        process_inner(
            &successor,
            0,
            ClientRequest::ChatPrepareResume {
                source: canonical.clone()
            }
        )
        .is_err()
    );
    {
        let state = successor.lock().unwrap();
        assert!(
            !state.chats.contains_key(&id),
            "unproved exit must never fall back to live adoption"
        );
        assert!(
            state.chats[&healthy_id].handle.same_actor(&healthy_handle),
            "unproved sibling must preserve the healthy actor's exact attachment"
        );
    }
    assert!(matches!(
        Handle::inspect_retained_retirement(&healthy_identity.record, &next).unwrap(),
        chat::runtime::RetirementInspection::Running
    ));
    let healthy_source = successor.lock().unwrap().chats[&healthy_id].session.clone();
    process(
        &successor,
        0,
        ClientRequest::ChatAttach {
            session: healthy_source,
        },
    )
    .unwrap();
    fs::rename(&held, &retired).unwrap();
    let mut wrong = canonical.clone();
    wrong
        .launch
        .recipe
        .arguments
        .push("--wrong-retired-recipe".into());
    assert!(super::native_cleanup::close_retained(&successor, &wrong).is_err());
    assert!(
        process_inner(
            &successor,
            0,
            ClientRequest::ChatPrepareResume { source: wrong }
        )
        .is_err()
    );
    if std::env::var(MODE).unwrap() == "cold-close" {
        super::native_cleanup::close_retained(&successor, &canonical).unwrap();
    } else {
        retained_actor::reconcile_retired_actors(&successor).unwrap();
    }
    let registry = super::super::agent_execution::registry(&successor).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .retained_agent_authority_source(id)
            .unwrap()
            .is_none(),
        "one cold-close or reconciliation pass must retire the exact source authority"
    );
    let retirement = registry
        .lock()
        .unwrap()
        .native_actor_retirement(identity.instance)
        .unwrap()
        .unwrap();
    assert_eq!(retirement.owner(), &identity.record);
    assert_eq!(
        retirement.confirmed_session(),
        canonical.launch.session_id.as_deref()
    );
    let ServerResponse::ChatResumePrepared { session: resumed } = process_inner(
        &successor,
        0,
        ClientRequest::ChatPrepareResume {
            source: canonical.clone(),
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_ne!(resumed.id, canonical.id);
    let registry = registry.lock().unwrap();
    let predecessor = registry
        .session_lineage(canonical.id)
        .unwrap()
        .unwrap_or(crate::agent_execution::Lineage(canonical.id));
    assert_eq!(
        registry.session_lineage(resumed.id).unwrap(),
        Some(predecessor)
    );
    drop(registry);
    assert_eq!(resumed.launch, canonical.launch);
    assert!(matches!(
        resumed.native_continuity,
        chat::NativeContinuity::ReconnectOnly
    ));
    process(
        &successor,
        0,
        ClientRequest::ChatClose {
            chat_id: healthy_id,
        },
    )
    .unwrap();
    wait(
        || {
            !healthy_identity.record.root.is_current()
                && !healthy_identity.record.holder.is_current()
        },
        "unrelated actor's exact original processes did not settle",
    );
    assert!(successor.lock().unwrap().chats.is_empty());
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(
        fs::read_to_string(root.join("produced"))
            .unwrap()
            .lines()
            .count(),
        40
    );
}
