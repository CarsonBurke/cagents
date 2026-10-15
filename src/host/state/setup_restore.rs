//! Upgrade recovery retains setup intent; only an explicit reviewed request
//! may execute the recipe again. Startup runs before clients are admitted.
use super::*;

pub(super) fn retain_pane(host: &SharedHost, source: &TerminalSession, recipe: &crate::model::LaunchRecipe) {
    let result = (|| -> Result<usize, String> {
        let registry = host.lock().unwrap_or_else(|p| p.into_inner()).execution_registry.clone()
            .ok_or("The workspace registry is unavailable")?;
        // SQLite commit and model hydration must not borrow HostState. The
        // accept loop has not begun, so no client publication is required.
        let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let mut retained = 0;
        for view in registry.model()?.views {
            for before in view.panes {
                let crate::model::CardKind::Terminal(saved) = &before.kind else { continue; };
                if saved.id != source.id || saved.launch != source.launch { continue; }
                let mut session = saved.clone();
                session.launch = recipe.clone();
                session.continuity = TerminalContinuity::RelaunchAvailable {
                    recipe: recipe.clone(), last_exit: None,
                };
                let mut pane = before.clone();
                pane.pending_terminal_start = false;
                pane.kind = crate::model::CardKind::Terminal(session);
                registry.apply(&ModelEdit::ChangeView {
                    scope: view.scope.clone(), expected_revision: registry.view_revision(&view.scope)?,
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
    use crate::{model::{BootId, CardId, CardKind, HostIncarnation, HostSessionId, LaunchRecipe},
        upgrade::handoff::{Carried, Checkpoint, Phase, Store, Terminal},
        workspace_model::views::{Pane, Scope}};

    fn proof(incarnation: &str) -> HostSessionProof {
        HostSessionProof { host_session_id: HostSessionId::new("setup-restore"),
            boot_id: BootId::new("fixture"), host_incarnation: HostIncarnation::new(incarnation) }
    }
    fn session(recipe: LaunchRecipe, proof: &HostSessionProof) -> TerminalSession {
        TerminalSession { id: TerminalId::new(), launch: recipe, transcript_path: None,
            resume_hint: None, owned_codex_listener: None, recovery_origin: None, predecessor: None,
            continuity: TerminalContinuity::DetachedSameBoot { host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(), host_incarnation: proof.host_incarnation.clone() } }
    }

    #[test]
    fn mixed_setup_and_shell_restore_preserves_setup_without_replay_and_keeps_shell_usable() {
        for (carried, has_pane) in [(false, true), (true, true), (false, false)] {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let root = directory.path();
            let old = proof("old");
            let shell = LaunchRecipe::shell(root.to_owned()).unwrap();
            let mut setup_recipe = shell.clone();
            setup_recipe.program = "/bin/sh".into();
            setup_recipe.arguments = vec!["-c".into(), "printf replayed >> setup-replayed".into()];
            setup_recipe.environment.insert("CAGENTS_WORKTREE_SETUP".into(), "1".into());
            setup_recipe.environment.insert("CAGENTS_WORKTREE_SETUP_ID".into(), crate::model::OperationId::new().to_string());
            let setup = session(setup_recipe.clone(), &old);
            assert!(crate::upgrade::recovery_recipe(&setup).is_err(), "retained evidence must not authorize replay");
            let ordinary = session(shell.clone(), &old);
            let mut checkpoint = Checkpoint::new(old, vec![Terminal { source: ordinary.clone(),
                previous_sources: Vec::new(), relaunches: Vec::new(), recipe: shell, options: Default::default() }],
                Vec::new(), Vec::new());
            if carried {
                checkpoint.carried.push(Carried { source: setup.clone(), previous_sources: Vec::new(),
                    relaunches: Vec::new(), options: Default::default() });
                checkpoint.preserve_carried = true;
            } else {
                checkpoint.terminals.insert(0, Terminal { source: setup.clone(), previous_sources: Vec::new(),
                    relaunches: Vec::new(), recipe: setup_recipe.clone(), options: Default::default() });
                let mut altered = checkpoint.clone();
                altered.terminals[0].recipe.arguments.push("changed".into());
                assert!(altered.validate().is_err(), "even retained setup evidence must keep its exact recipe");
            }
            checkpoint.phase = Phase::Ready;
            let store = Store::open(&root.join("checkpoint")).unwrap();
            store.save(&checkpoint).unwrap();
            let mut state = HostState::new(proof("replacement"));
            if has_pane {
            let mut registry = super::super::super::model::Registry::at(&root.join("model.sqlite3")).unwrap();
            registry.apply(&ModelEdit::OpenProject { repository: root.to_owned(), name: "setup".into(), kind: Default::default() }).unwrap();
            let scope = Scope::Worktree { repository: root.to_owned(), path: root.to_owned() };
            registry.apply(&ModelEdit::SeedView { scope, closed: Vec::new(), panes: vec![Pane {
                id: CardId::new(), title: "Worktree setup".into(), user_title: None,
                pending_terminal_start: true, width_fraction: 1.0, scope: Some(root.to_owned()),
                kind: CardKind::Terminal(setup.clone()),
            }] }).unwrap();
                state.install_model(registry);
            }
            let host = Arc::new(HostLock::new(state));
            let proc_root = root.join("proc");
            fs::create_dir(&proc_root).unwrap();
            restore_upgrade_from(&host, store, &proc_root, None).unwrap();
            let handle = {
                let state = host.lock().unwrap();
                assert!(!state.terminals.contains_key(&setup.id));
                if has_pane {
                    let pane = state.read_model().unwrap().views.remove(0).panes.remove(0);
                    assert!(!pane.pending_terminal_start);
                    let CardKind::Terminal(retained) = pane.kind else { panic!("setup pane was lost"); };
                    assert_eq!(retained.launch, setup_recipe);
                    assert!(matches!(retained.continuity, TerminalContinuity::RelaunchAvailable { last_exit: None, .. }));
                }
                state.terminals[&ordinary.id].handle.clone()
            };
            handle.input(b"printf ordinary-restored > ordinary-restored\n".to_vec()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !root.join("ordinary-restored").exists() {
                assert!(Instant::now() < deadline, "ordinary terminal was not restored alongside setup");
                thread::sleep(Duration::from_millis(10));
            }
            handle.shutdown().unwrap();
            assert!(!root.join("setup-replayed").exists());
            let store = Store::open(&root.join("checkpoint")).unwrap();
            assert!(store.load().unwrap().is_none(), "archived setup evidence is not automatically replayed");
            let archives = fs::read_dir(root.join("checkpoint")).unwrap().flatten()
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("daemon-upgrade.interrupted-setup.")).collect::<Vec<_>>();
            assert_eq!(archives.len(), 1);
            let saved: serde_json::Value = serde_json::from_slice(&fs::read(archives[0].path()).unwrap()).unwrap();
            let entries = if carried { &saved["carried"] } else { &saved["terminals"] };
            assert!(entries.as_array().unwrap().iter().any(|entry| entry["source"]["id"] == setup.id.to_string()));
            restore_upgrade_from(&host, store, &proc_root, None).unwrap();
            assert!(!root.join("setup-replayed").exists());
        }
    }
}
