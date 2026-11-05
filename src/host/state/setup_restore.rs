//! Upgrade recovery retains setup intent; only an explicit reviewed request
//! may execute the recipe again. Startup runs before clients are admitted.
use super::*;

pub(super) fn retain_pane(
    host: &SharedHost,
    source: &TerminalSession,
    recipe: &crate::model::LaunchRecipe,
) {
    let result = (|| -> Result<usize, String> {
        let registry = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .execution_registry
            .clone()
            .ok_or("The workspace registry is unavailable")?;
        // SQLite commit and model hydration must not borrow HostState. The
        // accept loop has not begun, so no client publication is required.
        let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let mut retained = 0;
        for view in registry.model()?.views {
            for before in view.panes {
                let crate::model::CardKind::Terminal(saved) = &before.kind else {
                    continue;
                };
                if saved.id != source.id || saved.launch != source.launch {
                    continue;
                }
                let mut session = saved.clone();
                session.launch = recipe.clone();
                session.continuity = TerminalContinuity::RelaunchAvailable {
                    recipe: recipe.clone(),
                    last_exit: None,
                };
                let mut pane = before.clone();
                pane.pending_terminal_start = false;
                pane.kind = crate::model::CardKind::Terminal(session);
                let expected_revision = registry.view_revision(&view.scope)?;
                registry.apply(&ModelEdit::ChangeView {
                    scope: view.scope.clone(),
                    expected_revision,
                    operation: uuid::Uuid::new_v4(),
                    change: crate::workspace_model::views::Change::Update { before, pane },
                })?;
                retained += 1;
            }
        }
        Ok(retained)
    })();
    match result {
        Ok(panes) => tracing::warn!(terminal = %source.id, panes,
            "Worktree setup was interrupted; its pane is retained for explicit reviewed resume"),
        Err(error) => tracing::warn!(terminal = %source.id, %error,
            "Interrupted setup pane could not be updated; its upgrade checkpoint will be preserved for explicit recovery"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{BootId, CardId, CardKind, HostIncarnation, HostSessionId, LaunchRecipe},
        upgrade::handoff::{Carried, Checkpoint, Phase, Store, Terminal},
        workspace_model::views::{Pane, Scope},
    };

    fn isolated_fixture(name: &str) -> bool {
        use crate::process::ProcessExecutor;
        // Libtest omits the crate prefix included by module_path!.
        let name = name
            .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
            .unwrap_or(name);
        const MARKER: &str = "CAGENTS_SETUP_RESTORE_FIXTURE";
        if std::env::var_os(MARKER).as_deref() == Some(std::ffi::OsStr::new(name)) {
            return false;
        }
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        for directory in ["home", "run", "config", "data", "cache"] {
            crate::secure_file::prepare_private_directory(&root.path().join(directory)).unwrap();
        }
        let mut command = crate::process::CommandSpec::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"]);
        for (key, _) in std::env::vars_os() {
            command = command.env_remove(key);
        }
        command = command
            .env(MARKER, name)
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", "/bin/sh")
            .env("HOME", root.path().join("home"))
            .env("XDG_RUNTIME_DIR", root.path().join("run"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CACHE_HOME", root.path().join("cache"));
        command.timeout = Some(Duration::from_secs(45));
        let output = crate::process::SystemProcessExecutor
            .execute(&command, &crate::repository::CancellationToken::new())
            .unwrap();
        assert!(
            output.success,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed; 0 failed"),
            "the isolated contract must actually execute: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        println!("{}", String::from_utf8_lossy(&output.stdout));
        true
    }

    struct CloseFixtureTerminals(SharedHost);
    impl Drop for CloseFixtureTerminals {
        fn drop(&mut self) {
            let handles = self
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .terminals
                .values()
                .map(|record| record.handle.clone())
                .collect::<Vec<_>>();
            for handle in handles {
                let _ = handle.shutdown();
            }
        }
    }

    struct CloseFixtureHandle(TerminalHandle);
    impl Drop for CloseFixtureHandle {
        fn drop(&mut self) {
            let _ = self.0.shutdown();
        }
    }

    // Only exact processes created by this private fixture enter this guard.
    // Authenticate again after opening the pidfd, then keep that same kernel
    // handle through cleanup. A mismatched capture is closed without signalling.
    #[cfg(target_os = "linux")]
    #[derive(Default)]
    struct FixtureProcesses {
        descriptors: Vec<std::os::fd::OwnedFd>,
        original_released: bool,
    }
    #[cfg(target_os = "linux")]
    impl FixtureProcesses {
        fn retain(
            &mut self,
            process: &crate::daemon::holder_registry::Process,
        ) -> std::io::Result<()> {
            self.retain_after_open(process, || {
                crate::daemon::holder_registry::Process::read(process.pid)
            })
        }
        fn retain_after_open(
            &mut self,
            process: &crate::daemon::holder_registry::Process,
            read: impl FnOnce() -> std::io::Result<crate::daemon::holder_registry::Process>,
        ) -> std::io::Result<()> {
            use std::os::fd::FromRawFd;
            if !process.is_current() {
                return Err(std::io::Error::other(
                    "fixture process identity changed before capture",
                ));
            }
            let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, process.pid, 0) as i32 };
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let descriptor = unsafe { std::os::fd::OwnedFd::from_raw_fd(descriptor) };
            if read()? != *process {
                return Err(std::io::Error::other(
                    "fixture process identity changed during capture",
                ));
            }
            self.descriptors.push(descriptor);
            Ok(())
        }
    }
    #[cfg(target_os = "linux")]
    impl Drop for FixtureProcesses {
        fn drop(&mut self) {
            use std::os::fd::AsRawFd;
            for (index, descriptor) in self.descriptors.iter().enumerate() {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        descriptor.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                    let mut ready = libc::pollfd {
                        fd: descriptor.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    libc::poll(&mut ready, 1, 2000);
                    // Until carry joins, the original worker still owns its
                    // Child and its wait. After carry relinquished it, only
                    // this exact originally captured holder may need reaping.
                    if self.original_released && index == 1 {
                        let mut status: libc::siginfo_t = std::mem::zeroed();
                        libc::waitid(
                            libc::P_PIDFD,
                            descriptor.as_raw_fd() as libc::id_t,
                            &mut status,
                            libc::WEXITED | libc::WNOHANG,
                        );
                    }
                }
            }
        }
    }

    fn proof(incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new("setup-restore"),
            boot_id: BootId::new("fixture"),
            host_incarnation: HostIncarnation::new(incarnation),
        }
    }
    fn session(recipe: LaunchRecipe, proof: &HostSessionProof) -> TerminalSession {
        TerminalSession {
            id: TerminalId::new(),
            launch: recipe,
            transcript_path: None,
            resume_hint: None,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            },
        }
    }

    #[test]
    fn mixed_setup_and_shell_restore_preserves_setup_without_replay_and_keeps_shell_usable() {
        if isolated_fixture(concat!(
            module_path!(),
            "::mixed_setup_and_shell_restore_preserves_setup_without_replay_and_keeps_shell_usable"
        )) {
            return;
        }
        for (carried, has_pane) in [(false, true), (true, true), (false, false)] {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let root = directory.path();
            let old = proof("old");
            let shell = LaunchRecipe::shell(root.to_owned()).unwrap();
            let mut setup_recipe = shell.clone();
            setup_recipe.program = "/bin/sh".into();
            setup_recipe.arguments = vec!["-c".into(), "printf replayed >> setup-replayed".into()];
            setup_recipe
                .environment
                .insert("CAGENTS_WORKTREE_SETUP".into(), "1".into());
            setup_recipe.environment.insert(
                "CAGENTS_WORKTREE_SETUP_ID".into(),
                crate::model::OperationId::new().to_string(),
            );
            let setup = session(setup_recipe.clone(), &old);
            assert!(
                crate::upgrade::recovery_recipe(&setup).is_err(),
                "retained evidence must not authorize replay"
            );
            let ordinary = session(shell.clone(), &old);
            let mut checkpoint = Checkpoint::new(
                old,
                vec![Terminal {
                    source: ordinary.clone(),
                    previous_sources: Vec::new(),
                    relaunches: Vec::new(),
                    recipe: shell,
                    options: Default::default(),
                }],
                Vec::new(),
                Vec::new(),
            );
            if carried {
                checkpoint.carried.push(Carried {
                    source: setup.clone(),
                    previous_sources: Vec::new(),
                    relaunches: Vec::new(),
                    options: Default::default(),
                });
                checkpoint.preserve_carried = true;
            } else {
                checkpoint.terminals.insert(
                    0,
                    Terminal {
                        source: setup.clone(),
                        previous_sources: Vec::new(),
                        relaunches: Vec::new(),
                        recipe: setup_recipe.clone(),
                        options: Default::default(),
                    },
                );
                let mut altered = checkpoint.clone();
                altered.terminals[0].recipe.arguments.push("changed".into());
                assert!(
                    altered.validate().is_err(),
                    "even retained setup evidence must keep its exact recipe"
                );
            }
            checkpoint.phase = Phase::Ready;
            let store = Store::open(&root.join("checkpoint")).unwrap();
            store.save(&checkpoint).unwrap();
            let mut state = HostState::new(proof("replacement"));
            if has_pane {
                let mut registry =
                    super::super::super::model::Registry::at(&root.join("model.sqlite3")).unwrap();
                registry
                    .apply(&ModelEdit::OpenProject {
                        repository: root.to_owned(),
                        name: "setup".into(),
                        kind: Default::default(),
                    })
                    .unwrap();
                let scope = Scope::Worktree {
                    repository: root.to_owned(),
                    path: root.to_owned(),
                };
                registry
                    .apply(&ModelEdit::SeedView {
                        scope,
                        closed: Vec::new(),
                        panes: vec![Pane {
                            id: CardId::new(),
                            title: "Worktree setup".into(),
                            user_title: None,
                            pending_terminal_start: true,
                            width_fraction: 1.0,
                            scope: Some(root.to_owned()),
                            kind: CardKind::Terminal(setup.clone()),
                        }],
                    })
                    .unwrap();
                state.install_model(registry);
            }
            let host = Arc::new(HostLock::new(state));
            let _cleanup = CloseFixtureTerminals(host.clone());
            let proc_root = root.join("proc");
            fs::create_dir(&proc_root).unwrap();
            let missing_holders = root.join("missing-holders");
            restore_upgrade_from(
                &host,
                store,
                &proc_root,
                carried.then_some(missing_holders.as_path()),
            )
            .unwrap();
            let handle = {
                let state = host.lock().unwrap();
                assert!(!state.terminals.contains_key(&setup.id));
                if has_pane {
                    let pane = state.read_model().unwrap().views.remove(0).panes.remove(0);
                    assert!(!pane.pending_terminal_start);
                    let CardKind::Terminal(retained) = pane.kind else {
                        panic!("setup pane was lost");
                    };
                    assert_eq!(retained.launch, setup_recipe);
                    assert!(matches!(
                        retained.continuity,
                        TerminalContinuity::RelaunchAvailable {
                            last_exit: None,
                            ..
                        }
                    ));
                }
                state.terminals[&ordinary.id].handle.clone()
            };
            handle
                .input(b"printf ordinary-restored > ordinary-restored\n".to_vec())
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !root.join("ordinary-restored").exists() {
                assert!(
                    Instant::now() < deadline,
                    "ordinary terminal was not restored alongside setup"
                );
                thread::sleep(Duration::from_millis(10));
            }
            handle.shutdown().unwrap();
            assert!(!root.join("setup-replayed").exists());
            let store = Store::open(&root.join("checkpoint")).unwrap();
            assert!(
                store.load().unwrap().is_none(),
                "archived setup evidence is not automatically replayed"
            );
            let archives = fs::read_dir(root.join("checkpoint"))
                .unwrap()
                .flatten()
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("daemon-upgrade.interrupted-setup.")
                })
                .collect::<Vec<_>>();
            assert_eq!(archives.len(), 1);
            let saved: serde_json::Value =
                serde_json::from_slice(&fs::read(archives[0].path()).unwrap()).unwrap();
            let entries = if carried {
                &saved["carried"]
            } else {
                &saved["terminals"]
            };
            assert!(
                entries
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["source"]["id"] == setup.id.to_string())
            );
            restore_upgrade_from(&host, store, &proc_root, None).unwrap();
            assert!(!root.join("setup-replayed").exists());
        }
    }

    #[test]
    fn marked_setup_generic_spawns_refuse_before_pending_reservation_or_command_execution() {
        if isolated_fixture(concat!(
            module_path!(),
            "::marked_setup_generic_spawns_refuse_before_pending_reservation_or_command_execution"
        )) {
            return;
        }
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = directory.path();
        let mut recipe = LaunchRecipe::shell(root.to_owned()).unwrap();
        recipe.program = "/bin/sh".into();
        recipe.arguments = vec!["-c".into(), "printf replayed >> forbidden-replay".into()];
        recipe
            .environment
            .insert("CAGENTS_WORKTREE_SETUP".into(), "1".into());
        let source = session(recipe, &proof("old"));
        let mut registry =
            super::super::super::model::Registry::at(&root.join("model.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: root.to_owned(),
                name: "setup".into(),
                kind: Default::default(),
            })
            .unwrap();
        let pane = Pane {
            id: CardId::new(),
            title: "setup".into(),
            user_title: None,
            pending_terminal_start: true,
            width_fraction: 1.0,
            scope: Some(root.to_owned()),
            kind: CardKind::Terminal(source.clone()),
        };
        registry
            .apply(&ModelEdit::SeedView {
                scope: Scope::Worktree {
                    repository: root.to_owned(),
                    path: root.to_owned(),
                },
                panes: vec![pane.clone()],
                closed: Vec::new(),
            })
            .unwrap();
        let mut state = HostState::new(proof("current"));
        state.install_model(registry);
        state.register_client(1);
        let host = Arc::new(HostLock::new(state));
        let _cleanup = CloseFixtureTerminals(host.clone());
        for error in [
            spawn_terminal(&host, 1, source.clone(), Default::default()).unwrap_err(),
            spawn_pending_terminal(&host, 1, pane.id, source.id, Default::default()).unwrap_err(),
        ] {
            assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
            assert!(error.message.contains("review and explicitly resume"));
        }
        let state = host.lock().unwrap();
        assert!(state.terminals.is_empty());
        let saved = state.read_model().unwrap().views.remove(0).panes.remove(0);
        assert!(
            saved.pending_terminal_start,
            "a refused generic request cannot consume the pending pane"
        );
        assert_eq!(saved.kind, pane.kind);
        assert!(!root.join("forbidden-replay").exists());
        println!("SETUP_GENERIC_REPLAY_REFUSED");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fixture_pidfd_capture_refuses_changed_identity_before_arming_cleanup() {
        if isolated_fixture(concat!(
            module_path!(),
            "::fixture_pidfd_capture_refuses_changed_identity_before_arming_cleanup"
        )) {
            return;
        }
        let original =
            crate::daemon::holder_registry::Process::read(std::process::id() as i32).unwrap();
        let mut changed = original.clone();
        changed.start = changed.start.checked_add(1).unwrap();
        let mut processes = FixtureProcesses::default();
        let error = processes
            .retain_after_open(&original, || Ok(changed))
            .unwrap_err();
        assert!(error.to_string().contains("during capture"));
        assert!(
            processes.descriptors.is_empty(),
            "an unauthenticated pidfd must be close-only, never enrolled for signals"
        );
        drop(processes);
        assert!(
            original.is_current(),
            "refused fixture capture cannot signal its target"
        );
        println!("SETUP_FIXTURE_PIDFD_IDENTITY_REFUSED");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn authenticated_carried_setup_reattaches_the_original_holder_without_replaying_its_recipe() {
        if isolated_fixture(concat!(
            module_path!(),
            "::authenticated_carried_setup_reattaches_the_original_holder_without_replaying_its_recipe"
        )) {
            return;
        }
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = directory.path();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let mut old = proof("old");
        old.boot_id = crate::daemon::read_boot_id().unwrap();
        let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &old).unwrap());
        let mut recipe = LaunchRecipe::shell(root.to_owned()).unwrap();
        recipe.program = "/bin/sh".into();
        recipe.arguments = vec![
            "-c".into(),
            "printf once >> setup-count; exec /bin/cat".into(),
        ];
        recipe
            .environment
            .insert("CAGENTS_WORKTREE_SETUP".into(), "1".into());
        let source = session(recipe, &old);
        // The fixture itself explicitly starts the initial trusted command;
        // the host restore path below may only adopt this already-running owner.
        let original = crate::terminal::TerminalHandle::spawn(
            source.clone(),
            crate::terminal::TerminalConfig {
                holder_recovery: Some(crate::terminal::holder::Recovery {
                    source: source.clone(),
                    previous_sources: Vec::new(),
                    relaunches: Vec::new(),
                    options: Default::default(),
                    execution_environment: None,
                    execution_recipe: None,
                }),
                ..Default::default()
            },
            |_| {},
        )
        .unwrap();
        let _original_cleanup = CloseFixtureHandle(original.clone());
        let owner = original.holder_identity().unwrap().clone();
        let mut processes = FixtureProcesses::default();
        processes.retain(&owner.root).unwrap();
        processes.retain(&owner.holder).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fs::read(root.join("setup-count")).is_ok_and(|bytes| bytes == b"once") {
            assert!(
                Instant::now() < deadline,
                "the initial reviewed fixture command did not run"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let holders = crate::terminal::holder::directory().unwrap();
        original
            .carry(crate::terminal::holder::socket(&holders, source.id))
            .unwrap();
        processes.original_released = true;
        let mut checkpoint = Checkpoint::new(old.clone(), Vec::new(), Vec::new(), Vec::new());
        checkpoint.carried.push(Carried {
            source: source.clone(),
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            options: Default::default(),
        });
        checkpoint.preserve_carried = true;
        checkpoint.phase = Phase::Ready;
        let store = Store::open(&root.join("checkpoint")).unwrap();
        store.save(&checkpoint).unwrap();
        drop(lease.take());
        let mut current = old;
        current.host_incarnation = HostIncarnation::new("replacement");
        let _lease = crate::daemon::HostLease::acquire(&paths, &current).unwrap();
        let mut state = HostState::new(current);
        // Original-holder restoration also restores durable execution
        // authority, just as production startup does after opening its model.
        state.install_model(
            super::super::super::model::Registry::at(&root.join("model.sqlite3")).unwrap(),
        );
        assert!(state.execution_registry.is_some());
        state.enable_holder_recovery();
        let host = Arc::new(HostLock::new(state));
        let _cleanup = CloseFixtureTerminals(host.clone());
        restore_upgrade_from(&host, store, std::path::Path::new("/proc"), Some(&holders)).unwrap();
        let adopted = host.lock().unwrap().terminals[&source.id].handle.clone();
        assert_eq!(adopted.holder_identity(), Some(&owner));
        assert!(!adopted.is_completed());
        assert_eq!(
            fs::read_to_string(root.join("setup-count")).unwrap(),
            "once"
        );
        assert_eq!(
            adopted.close_checked(true),
            crate::terminal_ipc::TerminalCloseResult::Closed
        );
        assert_eq!(
            fs::read_to_string(root.join("setup-count")).unwrap(),
            "once"
        );
        assert!(
            Store::open(&root.join("checkpoint"))
                .unwrap()
                .load()
                .unwrap()
                .is_none()
        );
        println!("SETUP_CARRIED_HOLDER_REATTACHED_ONCE");
    }
}
