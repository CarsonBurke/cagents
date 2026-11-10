//! Recover this update's acknowledged preparation without starting providers.
use super::state::{self, SharedHost};
use crate::upgrade::{
    handoff::{Carried, Store},
    recovery,
};
use std::collections::BTreeSet;

/// Returns true only for the one startup carrying a validated recovery receipt.
/// Ordinary startup keeps Prepared's existing refusal and replay guarantees.
pub(super) fn restore(host: &SharedHost) -> Result<bool, String> {
    let Some(mut record) = recovery::take_for_host()? else {
        return Ok(false);
    };
    let store = Store::discover()?;
    let checkpoint = store
        .load()?
        .ok_or("Acknowledged recovery checkpoint disappeared")?;
    if checkpoint.phase != crate::upgrade::handoff::Phase::Prepared
        || checkpoint.proof != record.source
    {
        return Err("Acknowledged recovery no longer names this Prepared checkpoint".into());
    }
    // This route must never call normal restore_upgrade: its terminal and
    // nonretained chat entries are recipe/replay authority only after Ready.
    for terminal in &checkpoint.carried {
        record.validate_continuity()?;
        if state::adopt_prepared_terminal(host, terminal) {
            record.adopted_terminals.push(terminal.source.id);
        }
        ensure_no_ambiguous_adoption(host)?;
    }
    for terminal in &checkpoint.terminals {
        record.validate_continuity()?;
        let original = Carried {
            source: terminal.source.clone(),
            previous_sources: terminal.previous_sources.clone(),
            relaunches: terminal.relaunches.clone(),
            options: terminal.options.clone(),
        };
        if state::adopt_prepared_terminal(host, &original) {
            record.adopted_terminals.push(terminal.source.id);
        }
        ensure_no_ambiguous_adoption(host)?;
    }
    for chat in &checkpoint.chats {
        record.validate_continuity()?;
        if !chat.source.native_continuity.retains_provider() {
            continue;
        }
        // restore_upgrade's retained branch authenticates and adopts the exact
        // still-running holder. It has no replacement-provider fallback.
        match super::chat::restore_upgrade(host, chat) {
            Ok(()) => record.adopted_chats.push(chat.source.id),
            Err(reason) => {
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .native_adoption_failures
                    .remember_upgrade(chat, &reason);
                tracing::warn!(chat = %chat.source.id, %reason, "incomplete update retained its conversation for explicit recovery; no provider was launched");
            }
        }
        ensure_no_ambiguous_adoption(host)?;
    }
    // Retained native adoption may also install its paired terminal. Account
    // for actual published owners after all adoption, including those pairs.
    let (terminals, chats, successor) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (
            state.terminals.keys().copied().collect::<BTreeSet<_>>(),
            state.chats.keys().copied().collect::<BTreeSet<_>>(),
            state.proof().clone(),
        )
    };
    record.adopted_terminals = terminals.iter().copied().collect();
    record.adopted_chats = chats.iter().copied().collect();
    record.paused_terminals = record
        .census
        .terminal_ids()
        .difference(&terminals)
        .copied()
        .collect();
    record.paused_chats = record
        .census
        .chats
        .iter()
        .filter(|source| !chats.contains(&source.id))
        .map(|source| source.id)
        .collect();
    // Preserve every original pane. A paused native owner opens as recorded
    // history through the existing stop ledger; neither the crash scanner nor
    // a pane reattachment may launch its provider behind the user's back.
    let mut paused = paused_record(
        record.source.clone(),
        &record.census,
        &record.paused_chats,
        record.paused_terminals.clone(),
    )?;
    if let Some(previous) = crate::upgrade::floor::load()?
        .records
        .iter()
        .find(|previous| previous.source == record.source)
    {
        paused.protocol = previous.protocol;
    }
    paused.save()?;
    // A close performed by the already-approved floor can retire its managed
    // pane in the old registry. Restore that saved presentation, never its run.
    super::model::floor::restore_presentations()?;
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.pause_update_terminals(&record.paused_terminals);
        if record
            .adopted_terminals
            .iter()
            .any(|id| !state.terminals.contains_key(id))
            || record
                .adopted_chats
                .iter()
                .any(|id| !state.chats.contains_key(id))
        {
            return Err(
                "A recovered owner disappeared before its original pane inventory was verified"
                    .into(),
            );
        }
    }
    let retained_runs = super::chat::retained_actor::retained_runs(host);
    if let Ok(registry) = super::agent_execution::registry(host) {
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .interrupt_unproven_agent_runs_except(
                crate::chat::now_ms().unwrap_or(0),
                &retained_runs,
            )?;
    }
    // The superseding record preserves Prepared byte-for-byte. It records this
    // one adoption's result; it never grants a later plain start new authority.
    record.finish(&successor)?;
    Ok(true)
}

fn paused_record(
    source: crate::model::HostSessionProof,
    census: &recovery::Census,
    paused_chats: &[crate::model::TerminalId],
    paused_terminals: Vec<crate::model::TerminalId>,
) -> Result<crate::upgrade::floor::Record, String> {
    let chats = census
        .chats
        .iter()
        .filter(|chat| paused_chats.contains(&chat.id))
        .cloned()
        .collect();
    let mut paused = crate::upgrade::floor::Record::new(source, chats, paused_terminals);
    for source in &paused.chats {
        let saved = census
            .snapshots
            .iter()
            .find(|saved| saved.source == *source)
            .cloned()
            .ok_or("The paused conversation lost its exact captured page")?;
        paused.snapshots.push(saved);
    }
    paused.aliases = census
        .aliases
        .iter()
        .filter(|alias| {
            paused
                .chats
                .iter()
                .any(|source| crate::upgrade::floor::same_allocation(source, alias))
        })
        .cloned()
        .collect();
    paused.presentations = census
        .presentations
        .iter()
        .filter(|presentation| paused.chats.contains(&presentation.session))
        .cloned()
        .collect();
    Ok(paused)
}

fn ensure_no_ambiguous_adoption(host: &SharedHost) -> Result<(), String> {
    if let Some(reason) = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .startup_recovery()
    {
        return Err(reason.to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::{self, Session},
        model::{
            BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, TerminalId,
        },
        terminal_ipc::{ClientRequest, ServerResponse},
    };
    use std::{
        fs,
        sync::Arc,
        time::{Duration, Instant},
    };

    #[test]
    fn captured_unnamed_managed_codex_pane_attaches_as_history_without_executing_its_provider() {
        const ROOT: &str = "CAGENTS_RECOVERY_HISTORY_TEST_ROOT";
        let root = if let Some(root) = std::env::var_os(ROOT) {
            std::path::PathBuf::from(root)
        } else {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            for name in [
                "home", "run", "data", "config", "cache", "project", "codex", "claude",
            ] {
                crate::secure_file::prepare_private_directory(&root.path().join(name)).unwrap();
            }
            let selector = std::thread::current().name().unwrap().to_owned();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
                .env(ROOT, root.path())
                .env("HOME", root.path().join("home"))
                .env("XDG_RUNTIME_DIR", root.path().join("run"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_CACHE_HOME", root.path().join("cache"))
                .env("CODEX_HOME", root.path().join("codex"))
                .env("CLAUDE_CONFIG_DIR", root.path().join("claude"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        };
        let proof = |epoch| HostSessionProof {
            host_session_id: HostSessionId::new("recovery-history"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new(epoch),
        };
        let source = Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: LaunchRecipe {
                    program: "/must-not-execute-a-recovery-provider".into(),
                    arguments: Vec::new(),
                    working_directory: root.join("project"),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some(uuid::Uuid::new_v4().to_string()),
            },
            proof: Some(proof("old")),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let mut captured = recovery::SnapshotCapture::default();
        captured
            .push(
                source.clone(),
                chat::Snapshot {
                    session_id: source.launch.session_id.clone(),
                    title: Some("Captured before prepare".into()),
                    items: vec![Arc::new(chat::Item {
                        id: "saved-answer".into(),
                        role: chat::Role::Assistant,
                        content: chat::ItemContent::Text {
                            text: "The original turn remains visible".into(),
                            thinking: false,
                            phase: Default::default(),
                        },
                        started_at: None,
                        at: None,
                    })],
                    ..Default::default()
                },
            )
            .unwrap();
        let mut alias = source.clone();
        alias.launch.session_id = None;
        let agent = crate::agent_execution::Agent {
            id: crate::model::AgentId::new(),
            name: None,
            origin: crate::agent_execution::Origin::User,
            spec: crate::agent_execution::Spec {
                provider: crate::agent::Provider::Codex,
                account: None,
                target: crate::agent_execution::Target {
                    repository: root.join("project"),
                    directory: root.join("project"),
                    placement: None,
                },
                surface: crate::agent_execution::Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: crate::workspace_model::views::Scope::Channel(crate::model::ChannelId::new()),
            session: Some(crate::agent_execution::HostedSession::Chat(alias.clone())),
            console: None,
            created_ms: 1,
            closed: false,
        };
        let pane = crate::workspace_model::views::Pane {
            id: crate::model::CardId::from_uuid(agent.id.as_uuid()),
            title: "Paused managed Codex".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            kind: crate::model::CardKind::Chat(alias.clone()),
            scope: Some(root.join("project")),
        };
        let registry_path = root.join("registry.sqlite3");
        let mut registry = super::super::model::Registry::at(&registry_path).unwrap();
        registry
            .apply(&crate::workspace_model::ModelEdit::OpenProject {
                repository: root.join("project"),
                name: "Recovery fixture".into(),
                kind: Default::default(),
            })
            .unwrap();
        let crate::workspace_model::views::Scope::Channel(channel) = &agent.placement else {
            unreachable!()
        };
        registry
            .apply(&crate::workspace_model::ModelEdit::CreateChannel {
                id: *channel,
                repository: root.join("project"),
                title: "Paused Codex".into(),
                title_source: Default::default(),
            })
            .unwrap();
        let db = rusqlite::Connection::open(&registry_path).unwrap();
        db.execute(
            "INSERT INTO agents(id,origin,record,session_id,closed) VALUES(?1,'user',?2,?3,0)",
            rusqlite::params![
                agent.id.to_string(),
                serde_json::to_string(&agent).unwrap(),
                source.id.to_string()
            ],
        )
        .unwrap();
        drop(db);
        registry
            .apply(&crate::workspace_model::ModelEdit::SeedView {
                scope: agent.placement.clone(),
                panes: vec![pane.clone()],
                closed: Vec::new(),
            })
            .unwrap();
        let mut presentation =
            crate::upgrade::floor::Record::new(proof("old"), vec![source.clone()], Vec::new());
        registry.capture_update_floor(&mut presentation).unwrap();
        assert_eq!(presentation.aliases, vec![alias.clone()]);
        assert_eq!(presentation.presentations.len(), 1);
        let census = recovery::Census {
            chats: vec![source.clone()],
            snapshots: captured.finish(),
            aliases: presentation.aliases,
            presentations: presentation.presentations,
            ..Default::default()
        };
        let paused = paused_record(proof("old"), &census, &[source.id], Vec::new()).unwrap();
        assert_eq!(paused.presentations[0].pane, pane);
        paused.save().unwrap();
        let ledger = crate::upgrade::floor::load().unwrap();
        assert_eq!(ledger.resume_source(&alias), Some(&source));
        assert_eq!(ledger.records[0].presentations[0].pane, pane);
        let mut initial = state::HostState::new(proof("new"));
        initial.install_model(registry);
        initial.register_client(1);
        let host = Arc::new(state::HostLock::new(initial));
        let resolved = super::super::chat::upgrade_resolution::resolve(&host, 1, &alias).unwrap();
        assert!(matches!(
            super::super::chat::process(&host, 1, ClientRequest::ChatAttach { session: resolved })
                .unwrap(),
            ServerResponse::ChatAttached { .. }
        ));
        let handle = host.lock().unwrap().chats[&source.id].handle.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        let snapshot = loop {
            let snapshot = handle.snapshot(None).unwrap();
            if snapshot.read_only_reason.is_some() {
                break snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "paused history did not initialize"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(snapshot.exited);
        assert_eq!(snapshot.title.as_deref(), Some("Captured before prepare"));
        assert!(
            snapshot
                .read_only_reason
                .as_deref()
                .unwrap()
                .contains("Paused after an incomplete update")
        );
        assert!(snapshot.items.iter().any(|item| matches!(&item.content, chat::ItemContent::Text { text, .. } if text == "The original turn remains visible")));
        assert!(handle.is_read_only_history());
        assert!(!handle.owns_work());
        assert_eq!(snapshot.session_id, source.launch.session_id);
        assert!(!fs::exists("/must-not-execute-a-recovery-provider").unwrap());
        handle.close().unwrap();
    }
}
