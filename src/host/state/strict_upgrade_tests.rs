//! Confirmed continuity cannot degrade into recipe execution during shutdown.
use super::*;
use crate::{
    daemon::holder_registry::Record,
    model::{HostIncarnation, HostSessionId},
    upgrade::handoff::{Phase, Store},
};

#[test]
fn confirmed_carry_failures_preserve_real_owners_through_shutdown() {
    const FIXTURE: &str = "CAGENTS_STRICT_UPGRADE_FIXTURE";
    const TEST: &str = "host::state::strict_upgrade_tests::confirmed_carry_failures_preserve_real_owners_through_shutdown";
    let Ok(root) = std::env::var(FIXTURE) else {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        for path in ["run", "home", "data", "config", "cache"] {
            fs::create_dir(root.path().join(path)).unwrap();
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env(FIXTURE, root.path())
            .env("HOME", root.path().join("home"))
            .env("XDG_RUNTIME_DIR", root.path().join("run"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("isolated strict upgrade exceeded its deadline");
            }
            thread::sleep(Duration::from_millis(10));
        }
    };
    let root = std::path::PathBuf::from(root);
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let mut proof = HostSessionProof {
        host_session_id: HostSessionId::new("strict-upgrade"),
        host_incarnation: HostIncarnation::new("original"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
    };
    let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
    struct Cleanup(Vec<Record>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for record in &self.0 {
                // These exact processes were created only by this isolated fixture.
                for process in [&record.holder, &record.root] {
                    if process.is_current() {
                        unsafe {
                            libc::kill(process.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
    }
    let mut cleanup = Cleanup(Vec::new());
    for (index, fault) in ["carry", "save", "none"].iter().enumerate() {
        let mut state = HostState::new(proof.clone());
        state.enable_holder_recovery();
        let host = Arc::new(Mutex::new(state));
        let launches = root.join(format!("launches-{index}"));
        let recipe = crate::model::LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec![
                "-c".into(),
                format!("echo launch >> '{}'; exec /bin/cat", launches.display()),
            ],
            working_directory: root.clone(),
            environment: BTreeMap::new(),
            unset_environment: Default::default(),
        };
        let source = TerminalSession {
            id: TerminalId::new(),
            launch: recipe.clone(),
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
            resume_hint: None,
            transcript_path: None,
            owned_codex_listener: None,
            recovery_origin: None,
        };
        spawn_terminal(&host, 0, source.clone(), TerminalSpawnOptions::default()).unwrap();
        let handle = host.lock().unwrap().terminals[&source.id].handle.clone();
        let owner = handle.holder_identity().unwrap().clone();
        cleanup.0.push(owner.clone());
        let checkpoint = Store::discover().unwrap().path.parent().unwrap().to_owned();
        let holders = holder_directory().unwrap();
        let capability = || {
            host.lock()
                .unwrap()
                .shutdown_preflight(Instant::now())
                .unwrap()
                .capability
        };
        let invalid = root.join(format!("not-a-directory-{index}"));
        fs::write(&invalid, "occupied").unwrap();
        let refusal = prepare_upgrade_with_carry_policy(
            &host,
            capability(),
            false,
            Some(&invalid),
            true,
            || Store::open(&checkpoint),
        )
        .unwrap_err();
        assert!(refusal.message.contains("not committed"));
        assert!(!host.lock().unwrap().shutting_down);
        assert!(owner.root.is_current());

        prepare_upgrade_with_carry_policy(&host, capability(), false, Some(&holders), true, || {
            Store::open(&checkpoint)
        })
        .unwrap();
        let store = Store::open(&checkpoint).unwrap();
        let prepared = store.load().unwrap().unwrap();
        assert!(prepared.terminals.is_empty());
        assert_eq!(prepared.carried.len(), 1);
        assert!(prepared.preserve_carried);
        let encoded: serde_json::Value =
            serde_json::from_slice(&fs::read(&store.path).unwrap()).unwrap();
        assert_eq!(encoded["version"], 4);
        if *fault == "carry" {
            // The real registered holder rejects a different detach endpoint.
            // Its original discovery remains intact for exact crash adoption.
            host.lock().unwrap().upgrade.as_mut().unwrap().holders =
                Some(root.join("wrong-holder-directory"));
        } else if *fault == "save" {
            fs::rename(&store.path, checkpoint.join("prepared-backup.json")).unwrap();
            fs::create_dir(&store.path).unwrap();
        }
        let (shutdown, _) = host.lock().unwrap().graceful_shutdown();
        let result = super::super::finish_chat_shutdown(&host);
        assert_eq!(result.is_err(), *fault != "none", "{result:?}");
        for handle in shutdown {
            let _ = handle.shutdown();
        }
        assert!(
            owner.root.is_current(),
            "{fault} failure released continuing process"
        );
        assert!(owner.holder.is_current());
        if *fault != "save" {
            let retained = store.load().unwrap().unwrap();
            assert!(
                retained.terminals.is_empty(),
                "carry failure must never create a restart recipe"
            );
            assert_eq!(
                retained.phase,
                if *fault == "none" {
                    Phase::Ready
                } else {
                    Phase::Prepared
                }
            );
        } else {
            let retained: crate::upgrade::handoff::Checkpoint =
                serde_json::from_slice(&fs::read(checkpoint.join("prepared-backup.json")).unwrap())
                    .unwrap();
            assert!(retained.terminals.is_empty());
            // Repair the injected disk obstruction without changing any
            // checkpoint evidence before exercising the real startup path.
            fs::remove_dir(&store.path).unwrap();
            fs::rename(checkpoint.join("prepared-backup.json"), &store.path).unwrap();
        }
        drop(host);
        drop(lease.take());
        proof.host_incarnation = HostIncarnation::new(format!("successor-{index}"));
        lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
        let mut state = HostState::new(proof.clone());
        state.enable_holder_recovery();
        state.install_model(
            crate::host::model::Registry::at(&root.join(format!("registry-{index}.sqlite3")))
                .unwrap(),
        );
        let successor = Arc::new(Mutex::new(state));
        restore_upgrade(&successor).unwrap();
        restore_orphaned_holders(&successor).unwrap();
        let adopted = successor.lock().unwrap().terminals[&source.id]
            .handle
            .clone();
        assert_eq!(adopted.holder_identity(), Some(&owner));
        assert_eq!(
            fs::read_to_string(&launches).unwrap(),
            "launch\n",
            "recipe was executed twice"
        );
        assert_eq!(
            adopted.close_checked(true),
            crate::terminal_ipc::TerminalCloseResult::Closed
        );
        assert!(
            !owner.root.is_current(),
            "explicit close must still reclaim the process"
        );
        // Losing the holder later is not consent to execute its recipe.
        // Exercise the same durable startup path, with no process surviving to
        // incidentally protect us through the legacy process census.
        store.save(&prepared).unwrap();
        let missing = Arc::new(Mutex::new(HostState::new(proof.clone())));
        let refusal = restore_upgrade(&missing).unwrap_err();
        assert!(
            refusal.contains("no recipe restart was authorized"),
            "{refusal}"
        );
        assert!(missing.lock().unwrap().terminals.is_empty());
        assert_eq!(fs::read_to_string(&launches).unwrap(), "launch\n");
        assert!(store.load().unwrap().unwrap().preserve_carried);
        let mut mislabeled: serde_json::Value =
            serde_json::from_slice(&fs::read(&store.path).unwrap()).unwrap();
        mislabeled["version"] = 3.into();
        fs::write(&store.path, serde_json::to_vec(&mislabeled).unwrap()).unwrap();
        assert!(
            store.load().is_err(),
            "older readers must refuse strict continuation evidence"
        );
        store.remove().unwrap();
    }
}
