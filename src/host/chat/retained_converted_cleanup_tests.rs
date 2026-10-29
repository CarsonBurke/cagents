//! Cleanup of an unpaired conversion startup preserves its historical terminal worker.
use super::*;
use std::{fs, path::Path};

#[test]
fn pending_converted_cleanup_preserves_independent_terminal_worker() {
    const CHILD: &str = "CAGENTS_PENDING_CONVERTED_CLEANUP_TEST";
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
    retained_actor_tests::install_provider(&root.join("bin/claude"));
    let proof = |name| HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("pending-converted-cleanup-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: crate::model::HostIncarnation::new(name),
    };
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let first = proof("pending-owner");
    let lease = crate::daemon::HostLease::acquire(&paths, &first).unwrap();
    let (original, notifications) = host(&root, first.clone());
    original.lock().unwrap().enable_holder_recovery();
    let id = TerminalId::new();
    let recipe = crate::model::LaunchRecipe {
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: root.join("project"),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let terminal = crate::terminal_ipc::TerminalDescriptor::relaunchable(id, recipe, None).session;
    let (descriptor, sentinel) = super::super::state::spawn_terminal_view_owned(
        &original,
        0,
        terminal,
        crate::terminal_ipc::TerminalSpawnOptions::default(),
        None,
        false,
    )
    .unwrap();
    let sentinel_identity = sentinel
        .holder_identity()
        .expect("fixture requires an actual independent terminal holder")
        .clone();
    struct Cleanup(Vec<crate::daemon::holder_registry::Record>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for record in &self.0 {
                for process in [&record.root, &record.holder] {
                    if process.is_current() {
                        unsafe {
                            libc::kill(process.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
    }
    let mut cleanup = Cleanup(vec![sentinel_identity.clone()]);
    let mut source = chat::fresh_session(
        Provider::Claude,
        id,
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    source.proof = Some(first.clone());
    // This constructor represents the issued conversion-startup cutpoint: its historical
    // terminal view remains issued, but the helper has no paired holder or
    // console. The independent terminal remains owned by the original HOST.
    source.terminal = Some(descriptor.session.clone());
    let registry = super::super::agent_execution::registry(&original).unwrap();
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
    let pending =
        Handle::start_remote_owned(spec, source.clone(), prepared, environment, |_| {}).unwrap();
    let identity = pending.actor_identity().unwrap();
    cleanup.0.push(identity.record.clone());
    let inspection = Handle::inspect_remote(&identity.record, first.clone()).unwrap();
    assert_eq!(inspection.source, source);
    assert!(!inspection.state.installed);
    assert!(inspection.paired_holder.is_none());
    assert!(inspection.execution_console.is_none());
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
    let next = proof("successor");
    let _lease = crate::daemon::HostLease::acquire(&paths, &next).unwrap();
    let (successor, _notifications) = host(&root, next.clone());
    let refusal = super::native_recovery::restore(&successor, 0, &source).unwrap_err();
    assert!(
        refusal.contains("pair lacks exact worker evidence"),
        "{refusal}"
    );
    assert!(!successor.lock().unwrap().chats.contains_key(&id));
    assert!(!successor.lock().unwrap().terminals.contains_key(&id));
    let ownership = registry
        .lock()
        .unwrap()
        .native_actor_ownership(identity.instance)
        .unwrap()
        .unwrap();
    assert!(ownership.is_pending());
    assert_eq!(ownership.owner(), &identity.record);
    assert_eq!(ownership.source(), &source);
    let before_close = Handle::inspect_remote(&identity.record, next.clone()).unwrap();
    assert!(!before_close.state.installed);
    assert!(before_close.paired_holder.is_none());
    assert!(before_close.execution_console.is_none());
    assert_eq!(
        before_close.source.terminal,
        Some(descriptor.session.clone())
    );
    // Keep both original startup owners alive until the actual HOST cleanup
    // completes. Startup EOF or dropping an old context cannot satisfy this.
    retained_actor::close_unattached(&successor, &source, Some(identity.record.clone())).unwrap();
    wait(
        || !identity.record.root.is_current() && !identity.record.holder.is_current(),
        "converted startup helper did not prove exact retirement",
    );
    assert!(sentinel_identity.root.is_current());
    assert!(sentinel_identity.holder.is_current());
    assert!(!sentinel.is_completed());
    assert_eq!(sentinel.holder_identity(), Some(&sentinel_identity));
    {
        let state = original.lock().unwrap();
        let current = state.terminals.get(&id).unwrap();
        assert!(current.handle.same_worker(&sentinel));
        assert_eq!(current.handle.holder_identity(), Some(&sentinel_identity));
    }
    assert!(!successor.lock().unwrap().chats.contains_key(&id));
    assert!(!successor.lock().unwrap().terminals.contains_key(&id));
    let retirement = registry
        .lock()
        .unwrap()
        .native_actor_retirement(identity.instance)
        .unwrap()
        .unwrap();
    assert!(retirement.is_pending());
    assert_eq!(
        retirement.source().native_continuity,
        chat::NativeContinuity::ReconnectOnly,
        "uninstalled cleanup cannot manufacture installed capability"
    );
    assert_eq!(retirement.owner(), &identity.record);
    assert!(retirement.source().terminal.is_none());
    let receipt = Handle::retired_actor_receipt(&identity.record, &next)
        .unwrap()
        .expect("cleanup must publish the exact helper retirement receipt");
    assert_eq!(receipt.identity, identity);
    assert!(
        !receipt.installed,
        "the execed helper never committed installation"
    );
    assert_eq!(&receipt.source, retirement.source());
    assert!(receipt.previous_sources.contains(&source));
    assert!(
        !root.join("prompts").exists(),
        "cleanup must never send provider input"
    );
    // Release only the now-retired helper's startup reference. The independent
    // sentinel then receives its own exact worker close, with the authentic
    // issued descriptor and current daemon lease; cleanup never substituted a
    // replacement terminal or used an absent process as its receipt.
    context.installed();
    drop(pending);
    sentinel
        .close_with_receipt(&descriptor.session, &next)
        .unwrap();
    wait(
        || !sentinel_identity.root.is_current() && !sentinel_identity.holder.is_current(),
        "independent sentinel explicit close did not release its exact holder",
    );
    drop(context);
    drop(notifications);
    drop(original);
}
