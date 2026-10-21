//! Exact pre-ID authority survives confirmation and HOST publication failure.
use super::*;
use std::{fs, path::Path};

#[test]
fn unpaired_fresh_authority_survives_confirmation_and_publication_crash() {
    const CHILD: &str = "CAGENTS_RETAINED_PUBLICATION_TEST";
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
        while !condition() {
            assert!(Instant::now() < deadline, "{reason}");
            thread::sleep(Duration::from_millis(20));
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
        host_session_id: crate::model::HostSessionId::new("publication-fixture"),
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
    let published = original.lock().unwrap().chats[&id].session.clone();
    assert!(published.terminal.is_none());
    assert!(published.launch.session_id.is_none());
    assert!(matches!(
        published.native_continuity,
        chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
    ));
    let registry = super::super::agent_execution::registry(&original).unwrap();
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .retained_agent_authority_source(id)
            .unwrap(),
        Some(crate::agent_execution::HostedSession::Chat(
            published.clone()
        ))
    );
    assert_eq!(
        Handle::inspect_remote(&identity.record, first.clone())
            .unwrap()
            .source,
        published,
        "the Registry authority must already be the helper's exact acknowledged source",
    );
    wait(
        || handle.attachment_snapshot().ready,
        "fresh provider did not initialize",
    );
    handle
        .command(chat::Command::Prompt {
            text: "One exact slow turn".into(),
            attachments: Vec::new(),
            mode: chat::PromptMode::Send,
        })
        .unwrap();
    wait(
        || root.join("provider-done").exists() && handle.confirmed_session_id().is_some(),
        "provider never confirmed its first turn",
    );
    let confirmed = handle.confirmed_session_id().unwrap();
    let inspection = Handle::inspect_remote(&identity.record, first).unwrap();
    assert!(
        inspection.source == published || inspection.previous_sources.contains(&published),
        "the exact published pre-ID authority must remain helper-retained after provider confirmation"
    );
    let old_proxy = handle.resource_lifetime();
    let old_host = Arc::downgrade(&original);
    drop(handle);
    drop(notifications);
    drop(original);
    drop(lease);
    wait(
        || old_host.strong_count() == 0 && !old_proxy(),
        "first HOST did not detach",
    );
    let second = proof("publication-failure");
    let lease = crate::daemon::HostLease::acquire(&paths, &second).unwrap();
    let (failed, notifications) = host(&root, second.clone());
    // Fault the real Registry transaction, after the exact helper update ACK.
    // No records/cursors are manufactured, and this transaction must roll back.
    let fault = rusqlite::Connection::open(root.join("registry.sqlite3")).unwrap();
    fault.execute_batch("CREATE TRIGGER reject_publication BEFORE UPDATE ON agent_authorities BEGIN SELECT RAISE(ABORT, 'fixture publication-before-registry failure'); END;").unwrap();
    let refusal = super::native_recovery::restore(&failed, 0, &inspection.source).unwrap_err();
    assert!(
        refusal.contains("fixture publication-before-registry failure"),
        "{refusal}"
    );
    assert!(!failed.lock().unwrap().chats.contains_key(&id));
    let after_ack = Handle::inspect_remote(&identity.record, second).unwrap();
    assert_eq!(after_ack.identity, identity);
    assert_eq!(
        after_ack.source.launch.session_id.as_deref(),
        Some(confirmed.as_str())
    );
    assert_eq!(
        after_ack.source.proof,
        Some(failed.lock().unwrap().proof().clone())
    );
    assert!(after_ack.previous_sources.contains(&published));
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .retained_agent_authority_source(id)
            .unwrap(),
        Some(crate::agent_execution::HostedSession::Chat(
            published.clone()
        ))
    );
    fault
        .execute_batch("DROP TRIGGER reject_publication;")
        .unwrap();
    drop(fault);
    let failed_host = Arc::downgrade(&failed);
    drop(notifications);
    drop(failed);
    drop(lease);
    wait(
        || failed_host.strong_count() == 0,
        "failed HOST did not detach before successor lease",
    );
    let third = proof("successor");
    let _lease = crate::daemon::HostLease::acquire(&paths, &third).unwrap();
    let (successor, _notifications) = host(&root, third.clone());
    // The real orphan discovery entry must now recover the same unpaired actor.
    restore_orphaned_native(&successor, &[]).unwrap();
    let handle = successor.lock().unwrap().chats[&id].handle.clone();
    assert_eq!(handle.actor_identity(), Some(identity.clone()));
    let recovered = successor.lock().unwrap().chats[&id].session.clone();
    assert!(recovered.terminal.is_none());
    assert_eq!(recovered.proof, Some(third));
    assert_eq!(
        recovered.launch.session_id.as_deref(),
        Some(confirmed.as_str())
    );
    let registry = super::super::agent_execution::registry(&successor).unwrap();
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .retained_agent_authority_source(id)
            .unwrap(),
        Some(crate::agent_execution::HostedSession::Chat(recovered))
    );
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1,
        "recovery must not resend the first prompt"
    );
    assert_eq!(
        fs::read_to_string(root.join("produced"))
            .unwrap()
            .lines()
            .count(),
        40
    );
    // An internal restore owns the actor without subscribing a window. Use
    // its public descriptor to establish the client attachment before close.
    let ServerResponse::ChatListed { sessions } =
        process(&successor, 0, ClientRequest::ChatList).unwrap()
    else {
        panic!("expected actual public chat inventory");
    };
    let listed = sessions
        .into_iter()
        .find(|session| session.id == id)
        .unwrap();
    assert_eq!(listed.launch.provider, Provider::Claude);
    assert_eq!(listed.launch.recipe, published.launch.recipe);
    assert_eq!(
        listed.proof.as_ref(),
        Some(successor.lock().unwrap().proof())
    );
    assert_eq!(
        listed.launch.session_id.as_deref(),
        Some(confirmed.as_str())
    );
    let ServerResponse::ChatAttached { session, snapshot } = process(
        &successor,
        0,
        ClientRequest::ChatAttach {
            session: listed.clone(),
        },
    )
    .unwrap() else {
        panic!("expected actual public chat attachment");
    };
    assert_eq!(session, listed);
    assert_eq!(snapshot.session_id.as_deref(), Some(confirmed.as_str()));
    assert_eq!(snapshot.native_continuity, session.native_continuity);
    assert_eq!(handle.actor_identity(), Some(identity.clone()));
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1,
        "public attachment must not replay the confirmed prompt"
    );
    process(&successor, 0, ClientRequest::ChatClose { chat_id: id }).unwrap();
    wait(
        || !identity.record.root.is_current() && !identity.record.holder.is_current(),
        "explicit close left the unpaired native owner alive",
    );
}
