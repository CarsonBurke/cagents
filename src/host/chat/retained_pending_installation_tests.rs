//! A real uninstalled startup owner transfers one coherent retained authority tuple.
use super::*;
use std::{fs, path::Path};

#[test]
fn pending_uninstalled_adoption_publishes_one_retained_authority_tuple() {
    const CHILD: &str = "CAGENTS_PENDING_INSTALLATION_TEST";
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
        host_session_id: crate::model::HostSessionId::new("pending-installation-fixture"),
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
    let inspection = Handle::inspect_remote(&identity.record, first).unwrap();
    assert!(!inspection.state.installed);
    assert_eq!(inspection.source, source);
    assert_eq!(
        pending.native_continuity(),
        chat::NativeContinuity::ReconnectOnly
    );
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_ownerships(None, 128)
            .unwrap()
            .is_empty()
    );
    original.lock().unwrap().native_actor_startups.insert(
        id,
        retained_actor::Startup {
            source: source.clone(),
            requested: source.clone(),
            context: context.clone(),
        },
    );
    // Keep the actual pending owner/control socket alive while replacing the
    // daemon lease. An EOF teardown cannot accidentally substitute for adoption.
    drop(lease);
    let next = proof("successor");
    let _lease = crate::daemon::HostLease::acquire(&paths, &next).unwrap();
    let (successor, _notifications) = host(&root, next.clone());
    super::native_recovery::restore(&successor, 0, &source).unwrap();
    let handle = successor.lock().unwrap().chats[&id].handle.clone();
    let hosted = successor.lock().unwrap().chats[&id].session.clone();
    assert_eq!(handle.actor_identity(), Some(identity.clone()));
    assert!(matches!(
        hosted.native_continuity,
        chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
    ));
    assert!(hosted.launch.session_id.is_none());
    assert_eq!(hosted.proof, Some(next.clone()));
    let registry = super::super::agent_execution::registry(&successor).unwrap();
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .retained_agent_authority_source(id)
            .unwrap(),
        Some(crate::agent_execution::HostedSession::Chat(hosted.clone())),
        "pending installation must not leave Registry at ReconnectOnly"
    );
    let inspection = Handle::inspect_remote(&identity.record, next.clone()).unwrap();
    assert!(inspection.state.installed);
    assert_eq!(
        inspection.source, hosted,
        "helper ACK and HOST publication must name the same tuple"
    );
    assert!(inspection.previous_sources.contains(&source));
    let ownerships = registry
        .lock()
        .unwrap()
        .native_actor_ownerships(None, 128)
        .unwrap();
    assert_eq!(ownerships.len(), 1);
    assert_eq!(ownerships[0].actor(), identity.instance);
    assert_eq!(ownerships[0].owner(), &identity.record);
    assert_eq!(ownerships[0].source(), &hosted);
    assert_eq!(ownerships[0].scope(), handle.launch_account_scope());
    assert!(
        pending.command(chat::Command::Refresh).is_err(),
        "the prior lease/epoch must not accept a command after adoption"
    );
    assert!(
        !root.join("prompts").exists(),
        "pending adoption must not invent or replay input"
    );
    // Successful HOST publication owns cleanup now. Release the old context's
    // startup reference without invoking its cleanup/wait-for-helper-exit API.
    context.installed();
    drop(pending);
    drop(context);
    drop(notifications);
    drop(original);
    assert!(identity.record.root.is_current());
    // Internal orphan adoption does not subscribe a client. Obtain the exact
    // public descriptor and attach before exercising the client close path.
    let ServerResponse::ChatListed { sessions } =
        process(&successor, 0, ClientRequest::ChatList).unwrap()
    else {
        panic!("expected actual public chat inventory");
    };
    let listed = sessions
        .into_iter()
        .find(|session| session.id == id)
        .unwrap();
    assert_eq!(listed, hosted);
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
    assert_eq!(snapshot.session_id, session.launch.session_id);
    assert_eq!(snapshot.native_continuity, session.native_continuity);
    assert_eq!(handle.actor_identity(), Some(identity.clone()));
    assert!(
        !root.join("prompts").exists(),
        "attachment must not invent provider input"
    );
    process(&successor, 0, ClientRequest::ChatClose { chat_id: id }).unwrap();
    wait(
        || !identity.record.root.is_current() && !identity.record.holder.is_current(),
        "pending actor's explicit close did not retire its exact owner",
    );
}
