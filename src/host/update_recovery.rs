//! Recover this update's acknowledged preparation without starting providers.
use super::{
    agent_execution, chat, model,
    state::{self, SharedHost},
};
use crate::upgrade::{
    handoff::{Carried, Store},
    recovery,
};
use std::collections::BTreeSet;

/// A settled outcome says only that this exact Prepared checkpoint has already
/// been handled. It supplies no launch recipes or renewed execution authority.
pub(super) fn restore_settled(host: &SharedHost) -> Result<bool, String> {
    let store = Store::discover()?;
    let daemon = std::env::current_exe().map_err(|error| error.to_string())?;
    restore_settled_from(host, &store.path, &daemon)
}

pub(super) fn restore_settled_from(
    host: &SharedHost,
    checkpoint: &std::path::Path,
    daemon: &std::path::Path,
) -> Result<bool, String> {
    let Some(settled) = recovery::classify_retained_prepared(checkpoint, daemon)? else {
        return Ok(false);
    };
    tracing::info!(checkpoint = %checkpoint.display(),
        "settled update checkpoint identified; adopting current holders and retaining paused history");
    restore_recovered(host, checkpoint, daemon, settled)
}

/// Ordinary startup restores only current, authenticated holders and the
/// current Ready checkpoint. Archived update evidence cannot recreate panes.
pub(super) fn restore_ordinary(host: &SharedHost) -> Result<(), String> {
    #[cfg(all(test, target_os = "linux"))]
    super::observe_fixture_startup("ordinary: previous boot history", None);
    chat::retained_actor::reconcile_previous_boot_history(host)?;
    #[cfg(all(test, target_os = "linux"))]
    super::observe_fixture_startup("ordinary: capture Ready", None);
    let ready_capture = crate::upgrade::recovery::verified::ready::capture()?;
    #[cfg(all(test, target_os = "linux"))]
    super::observe_fixture_startup("ordinary: restore upgrade checkpoint", None);
    let unattached = state::restore_upgrade(host)?;
    #[cfg(all(test, target_os = "linux"))]
    super::observe_fixture_startup("ordinary: restore terminal holders", None);
    state::restore_orphaned_holders(host)?;
    if let Some(detail) = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .startup_recovery()
    {
        return Err(detail.to_owned());
    }
    #[cfg(all(test, target_os = "linux"))]
    super::observe_fixture_startup("ordinary: restore native holders", None);
    chat::restore_orphaned_native(host, &unattached)?;
    if let Some(detail) = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .startup_recovery()
    {
        return Err(detail.to_owned());
    }
    #[cfg(all(test, target_os = "linux"))]
    super::observe_fixture_startup("ordinary: reconcile retired actors", None);
    chat::retained_actor::reconcile_retired_actors(host)?;
    #[cfg(all(test, target_os = "linux"))]
    super::observe_fixture_startup("ordinary: reconcile startup losses", None);
    chat::retained_actor::reconcile_startup_losses(host)?;
    #[cfg(all(test, target_os = "linux"))]
    super::observe_fixture_startup("ordinary: restore current presentations", None);
    model::floor::restore_presentations()?;
    #[cfg(all(test, target_os = "linux"))]
    super::observe_fixture_startup("ordinary: reconcile retained runs", None);
    let retained_runs = chat::retained_actor::retained_runs(host);
    // A committed acceptance is not proof of provider delivery. Until a
    // restored actor supplies exact run evidence, never authorize a resend.
    if let Ok(registry) = agent_execution::registry(host) {
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .interrupt_unproven_agent_runs_except(
                crate::chat::now_ms().unwrap_or(0),
                &retained_runs,
            )?;
    }
    let (proof, terminals, chats) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let chats = state
            .chats
            .values()
            .map(|record| {
                let mut session = record.session.clone();
                session.launch.session_id = record.handle.session_id();
                session.native_continuity = record.handle.native_continuity();
                session
            })
            .collect::<Vec<_>>();
        (state.proof().clone(), state.list(0), chats)
    };
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    #[cfg(all(test, target_os = "linux"))]
    super::observe_fixture_startup("ordinary: publish Ready proof", None);
    crate::upgrade::recovery::verified::ready::publish(
        ready_capture,
        &proof,
        &executable,
        &terminals,
        &chats,
    )?;
    #[cfg(all(test, target_os = "linux"))]
    super::observe_fixture_startup("ordinary: optional archived proof", None);
    publish_archived_proof(host);
    #[cfg(all(test, target_os = "linux"))]
    super::observe_fixture_startup("ordinary: complete", None);
    Ok(())
}

/// Observe authenticated normal restoration without changing its adoption,
/// stopped histories or presentations. Stale optional history cannot hold the
/// current service in recovery-only mode.
fn publish_archived_proof(host: &SharedHost) {
    let result = (|| -> Result<(), String> {
        let store = Store::discover()?;
        let daemon = std::env::current_exe().map_err(|error| error.to_string())?;
        let Some(settled) = recovery::archived_discovery(&store.path, &daemon)? else {
            return Ok(());
        };
        let (serving, terminals, chats, aliases) = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            if state.startup_state() != crate::terminal_ipc::StartupState::Ready {
                return Ok(());
            }
            (
                state.proof().clone(),
                state
                    .list(0)
                    .into_iter()
                    .map(recovery::proof_terminal_descriptor)
                    .collect::<Vec<_>>(),
                live_chat_descriptors(&state),
                state.upgrade_chats.clone(),
            )
        };
        recovery::verified::cold::publish(
            &store.path,
            &daemon,
            &settled,
            &serving,
            &terminals,
            &chats,
            &aliases,
        )
    })();
    if let Err(error) = result {
        tracing::warn!(%error, "optional archived recovery proof is unavailable; normal Ready restoration was kept");
    }
}

fn restore_recovered(
    host: &SharedHost,
    checkpoint: &std::path::Path,
    daemon: &std::path::Path,
    settled: recovery::AlreadyRecovered,
) -> Result<bool, String> {
    // Settlement proves the first receipt-bearing recovery already retained
    // its paused pages. A later close or prune must not be undone by replaying
    // that historical census. Only actual holders and current retention count.
    let ledger = crate::upgrade::floor::load()?;
    let preserved = settled
        .census
        .terminal_ids()
        .difference(&settled.paused_terminals.iter().copied().collect())
        .copied()
        .filter(|id| !ledger.blocks_id_except_source(*id, &settled.source))
        .collect();
    super::chat::retained_actor::reconcile_previous_boot_history(host)?;
    state::restore_orphaned_holders_preserving(host, &preserved)?;
    ensure_no_ambiguous_adoption(host)?;
    // This scans and authenticates current holders, including owners created
    // explicitly since the recovery. The historical census is not a live list.
    let preserved_chats = settled
        .census
        .chats
        .iter()
        .map(|chat| chat.id)
        .filter(|id| !settled.paused_chats.contains(id))
        .filter(|id| !ledger.blocks_id_except_source(*id, &settled.source))
        .collect();
    super::chat::restore_orphaned_native_preserving(host, &[], &preserved_chats)?;
    ensure_no_ambiguous_adoption(host)?;
    super::chat::retained_actor::reconcile_retired_actors(host)?;
    super::chat::retained_actor::reconcile_startup_losses(host)?;

    super::model::floor::restore_presentations()?;
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
    ensure_no_ambiguous_adoption(host)?;
    let (serving, terminals, chats, aliases) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let chats = state
            .chats
            .values()
            .filter(|record| !record.handle.is_read_only_history())
            .map(|record| {
                let mut session = record.session.clone();
                session.launch.session_id = record.handle.session_id();
                session.native_continuity = record.handle.native_continuity();
                session
            })
            .collect::<Vec<_>>();
        (
            state.proof().clone(),
            state
                .list(0)
                .into_iter()
                .map(recovery::proof_terminal_descriptor)
                .collect::<Vec<_>>(),
            chats,
            state.upgrade_chats.clone(),
        )
    };
    // Current ownership is freshly authenticated above. Failure to prove the
    // historical update inventory must not prevent remaining real owners from
    // serving after someone later closed a pane. The reader accepts only an
    // exact positive publication for this incarnation, never the old proof.
    if let Err(error) = recovery::verified::cold::publish(
        checkpoint, daemon, &settled, &serving, &terminals, &chats, &aliases,
    ) {
        tracing::warn!(%error, "the current service is available but the original update census remains unverified");
    }
    Ok(true)
}

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
    let (terminals, chats, successor, descriptors) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (
            state.terminals.keys().copied().collect::<BTreeSet<_>>(),
            state.chats.keys().copied().collect::<BTreeSet<_>>(),
            state.proof().clone(),
            live_chat_descriptors(&state),
        )
    };
    record.adopted_terminals = terminals.iter().copied().collect();
    record.adopted_chats = chats.iter().copied().collect();
    record.adopted_chat_descriptors = descriptors;
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
    // Publish the same immutable issued-chat lineage used by ordinary Ready
    // restoration. Verification readers require the exact authenticated live
    // descriptor, so this journal grants neither adoption nor replay authority.
    let recovered = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        checkpoint
            .chats
            .iter()
            .filter_map(|chat| {
                let record = state.chats.get(&chat.source.id)?;
                let mut replacement = record.session.clone();
                replacement.launch.session_id = record.handle.session_id();
                replacement.native_continuity = record.handle.native_continuity();
                Some(crate::upgrade::handoff::RecoveredChat {
                    evidence: chat.clone(),
                    replacement,
                    aliases: Vec::new(),
                })
            })
            .collect::<Vec<_>>()
    };
    store.record_recovered(&recovered)?;
    record.adopted_chat_descriptors = recovered
        .iter()
        .map(|entry| entry.replacement.clone())
        .collect();
    record.adopted_terminal_descriptors = host.lock().unwrap_or_else(|p| p.into_inner()).list(0);
    // The superseding record preserves Prepared byte-for-byte. It records this
    // one adoption's result; it never grants a later plain start new authority.
    record.finish(&successor)?;
    Ok(true)
}

fn live_chat_descriptors(state: &state::HostState) -> Vec<crate::chat::Session> {
    state
        .chats
        .values()
        .filter(|record| !record.handle.is_read_only_history())
        .map(|record| {
            let mut session = record.session.clone();
            session.launch.session_id = record.handle.session_id();
            session.native_continuity = record.handle.native_continuity();
            session
        })
        .collect()
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
    // Frozen descriptors can precede the provider's identity notification.
    // Only the immutable captured page may fill that missing identity; later
    // Agent, pane and receipt descriptors cannot teach history another thread.
    for (source, saved) in paused.chats.iter_mut().zip(&mut paused.snapshots) {
        if source.launch.session_id.is_none()
            && let Some(identity) = saved.snapshot.session_id.clone()
        {
            if identity.is_empty() || identity.len() > 1024 {
                return Err("The captured conversation identity is invalid".into());
            }
            if !paused.aliases.contains(source) {
                paused.aliases.push(source.clone());
            }
            source.launch.session_id = Some(identity);
            saved.source = source.clone();
            if let Some(reason) = &mut saved.limitation {
                *reason = reason.replace(
                    "No conversation identity was recorded; start a new conversation explicitly to continue.",
                    "Resume explicitly to inspect whether the recorded conversation can continue.",
                );
            }
        }
    }
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
    };
    use std::{
        fs,
        sync::Arc,
        time::{Duration, Instant},
    };

    fn seed_managed_pane(
        registry_path: &std::path::Path,
        directory: &std::path::Path,
        alias: &Session,
    ) -> (
        super::super::model::Registry,
        crate::workspace_model::views::Pane,
    ) {
        let agent = crate::agent_execution::Agent {
            id: crate::model::AgentId::new(),
            name: None,
            origin: crate::agent_execution::Origin::User,
            spec: crate::agent_execution::Spec {
                provider: alias.launch.provider,
                account: None,
                target: crate::agent_execution::Target {
                    repository: directory.to_owned(),
                    directory: directory.to_owned(),
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
            scope: Some(directory.to_owned()),
        };
        let mut registry = super::super::model::Registry::at(&registry_path).unwrap();
        registry
            .apply(&crate::workspace_model::ModelEdit::OpenProject {
                repository: directory.to_owned(),
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
                repository: directory.to_owned(),
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
                alias.id.to_string()
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
        (registry, pane)
    }

    #[test]
    fn paused_record_never_learns_missing_identity_from_later_alias() {
        let proof = |epoch: &str| HostSessionProof {
            host_session_id: "snapshot-only-fixture".into(),
            boot_id: "private-test-boot".into(),
            host_incarnation: epoch.into(),
        };
        let source = Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: vec![],
                    working_directory: "/fixture".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: None,
            },
            proof: Some(proof("source")),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        };
        let mut later = source.clone();
        later.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        let census = recovery::Census {
            chats: vec![source.clone()],
            aliases: vec![later.clone()],
            snapshots: vec![crate::upgrade::floor::StoppedSnapshot {
                source: source.clone(),
                snapshot: Default::default(),
                limitation: None,
            }],
            ..Default::default()
        };
        let paused = paused_record(proof("source"), &census, &[source.id], vec![]).unwrap();
        assert_eq!(paused.chats, vec![source.clone()]);
        assert_eq!(paused.snapshots[0].source, source);
        assert!(paused.chats[0].launch.session_id.is_none());
        assert!(!paused.aliases.contains(&later));
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SettledCase {
        OrdinaryClose,
        CanonicalClose,
        MissingAdopted,
        CapturedIdentity,
    }

    #[test]
    fn settled_prepared_restart_restores_codex_history_without_provider_replay() {
        settled_restart_case(crate::agent::Provider::Codex, SettledCase::OrdinaryClose);
    }
    #[test]
    fn settled_prepared_restart_restores_claude_history_without_provider_replay() {
        settled_restart_case(crate::agent::Provider::Claude, SettledCase::OrdinaryClose);
    }
    #[test]
    fn settled_prepared_restart_never_recreates_closed_codex_history_before_archival() {
        settled_restart_case(crate::agent::Provider::Codex, SettledCase::CanonicalClose);
    }
    #[test]
    fn settled_prepared_restart_never_recreates_closed_claude_history_before_archival() {
        settled_restart_case(crate::agent::Provider::Claude, SettledCase::CanonicalClose);
    }
    #[test]
    fn settled_prepared_restart_never_turns_missing_adopted_codex_into_history() {
        settled_restart_case(crate::agent::Provider::Codex, SettledCase::MissingAdopted);
    }
    #[test]
    fn settled_prepared_restart_never_turns_missing_adopted_claude_into_history() {
        settled_restart_case(crate::agent::Provider::Claude, SettledCase::MissingAdopted);
    }

    #[test]
    fn settled_prepared_restart_binds_missing_identity_only_from_captured_page() {
        settled_restart_case(
            crate::agent::Provider::Claude,
            SettledCase::CapturedIdentity,
        );
    }

    fn settled_restart_case(provider_kind: crate::agent::Provider, case: SettledCase) {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::PermissionsExt;

        const ROOT: &str = "CAGENTS_SETTLED_RESTART_TEST_ROOT";
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
        let hash = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
        let proof = |epoch| HostSessionProof {
            host_session_id: HostSessionId::new("settled-restart"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: HostIncarnation::new(epoch),
        };
        let provider_dir = root.join("provider");
        crate::secure_file::prepare_private_directory(&provider_dir).unwrap();
        let provider = provider_dir.join(provider_kind.executable());
        let sentinel = root.join("provider-was-replayed");
        fs::write(
            &provider,
            b"#!/bin/sh\nprintf replayed > \"$CAGENTS_RECOVERY_SENTINEL\"\nexit 91\n",
        )
        .unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
        let mut source = Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: provider_kind,
                recipe: LaunchRecipe {
                    program: provider.to_string_lossy().into_owned(),
                    arguments: if provider_kind == crate::agent::Provider::Codex {
                        vec!["--remote".into(), "unix://".into()]
                    } else {
                        Vec::new()
                    },
                    working_directory: root.join("project"),
                    environment: [(
                        "CAGENTS_RECOVERY_SENTINEL".into(),
                        sentinel.to_string_lossy().into_owned(),
                    )]
                    .into(),
                    unset_environment: Default::default(),
                },
                session_id: None,
            },
            proof: Some(proof("source")),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: if provider_kind == crate::agent::Provider::Claude {
                chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
            } else {
                Default::default()
            },
        };
        if provider_kind == crate::agent::Provider::Codex {
            assert!(source.claim_owned_codex_listener(&root.join("run"), &proof("source")));
        }
        let alias = source.clone();
        source.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        if provider_kind == crate::agent::Provider::Codex {
            source.native_continuity = chat::NativeContinuity::Durable {
                provider_version: "0.100.0".into(),
            };
        }
        let history_source = source.clone();
        if case == SettledCase::CapturedIdentity {
            source.launch.session_id = None;
        }
        let mut snapshots = recovery::SnapshotCapture::default();
        snapshots
            .push(
                source.clone(),
                chat::Snapshot {
                    session_id: history_source.launch.session_id.clone(),
                    title: Some("Paused across daemon restarts".into()),
                    items: vec![Arc::new(chat::Item {
                        id: "retained-page".into(),
                        role: chat::Role::Assistant,
                        content: chat::ItemContent::Text {
                            text: "The captured page survived".into(),
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
        let registry_path = crate::instance::project_dirs()
            .unwrap()
            .data_dir()
            .join("workspace-model.sqlite3");
        let (mut registry, pane) =
            seed_managed_pane(&registry_path, &root.join("project"), &source);
        let mut presentation =
            crate::upgrade::floor::Record::new(proof("source"), vec![source.clone()], Vec::new());
        registry.capture_update_floor(&mut presentation).unwrap();
        assert_eq!(presentation.aliases, vec![source.clone()]);
        assert_eq!(presentation.presentations[0].pane, pane);
        let census = recovery::Census {
            chats: vec![source.clone()],
            aliases: presentation.aliases,
            presentations: presentation.presentations,
            snapshots: snapshots.finish(),
            ..Default::default()
        };
        if case == SettledCase::MissingAdopted {
            // Model the descriptor B recorded by the completed first recovery.
            // No holder exists now; this evidence cannot manufacture history.
            let mut issued = source.clone();
            issued.proof = Some(proof("first-recovery"));
            assert!(
                registry
                    .adopt_authenticated_agent_session(
                        &[crate::agent_execution::HostedSession::Chat(source.clone())],
                        &crate::agent_execution::HostedSession::Chat(issued),
                    )
                    .unwrap()
            );
        } else {
            // The first recovery actually wrote this paused page before its
            // outcome became settled. Later startups may only read that page.
            let original = registry.agent_for_session(source.id).unwrap().unwrap();
            registry.close_agent(&original.origin, original.id).unwrap();
            paused_record(proof("source"), &census, &[source.id], Vec::new())
                .unwrap()
                .save()
                .unwrap();
            model::floor::restore_presentations().unwrap();
        }
        let original_agent = registry.agent_for_session(source.id).unwrap().unwrap();
        let Some(crate::agent_execution::HostedSession::Chat(latest_saved)) =
            &original_agent.session
        else {
            unreachable!()
        };
        let latest_saved = latest_saved.clone();
        assert_eq!(latest_saved.launch.session_id, source.launch.session_id);
        drop(registry);
        let store = Store::discover().unwrap();
        store
            .save(&crate::upgrade::handoff::Checkpoint::new(
                proof("source"),
                Vec::new(),
                Vec::new(),
                vec![crate::upgrade::handoff::Chat {
                    source: alias.clone(),
                    previous_sources: Vec::new(),
                    launch: source.launch.clone(),
                }],
            ))
            .unwrap();
        let checkpoint = fs::read(&store.path).unwrap();
        let checkpoint_hash = hash(&checkpoint);
        let prefix = root.join("install");
        let bin = prefix.join("versions/0.1.3+local.settled/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            prefix.join(".cagents-install-root"),
            b"cagents install root",
        )
        .unwrap();
        for binary in ["cagents", "cagents-ui", "cagentsd"] {
            fs::write(bin.join(binary), b"private matched binary").unwrap();
        }
        let daemon_binary = bin.join("cagentsd");
        let updates = prefix.join(".update");
        crate::secure_file::prepare_private_directory(&updates).unwrap();
        let job = uuid::Uuid::new_v4().to_string();
        let request =
            serde_json::to_vec(&serde_json::json!({"Installed":{"root":bin.parent().unwrap()}}))
                .unwrap();
        let request_path = updates.join(format!("{job}.request"));
        fs::write(&request_path, &request).unwrap();
        fs::set_permissions(&request_path, fs::Permissions::from_mode(0o600)).unwrap();
        let reviewed = crate::update::impact::DaemonImpact {
            capability: crate::terminal_ipc::ShutdownCapability {
                proof: proof("source"),
                token: "reviewed-before-prepare".into(),
                inventory_generation: 1,
                live_terminals: 1,
                background_subagents: 0,
                background_identity: String::new(),
                expires_at_unix_ms: 100,
            },
            policy: crate::update::impact::Policy::WhenIdle,
            sessions: vec![crate::update::impact::SessionImpact {
                id: source.id,
                kind: crate::update::impact::SessionKind::NativeChat,
                title: "Paused Codex".into(),
                identity: hash(&serde_json::to_vec(&source).unwrap()),
                disposition: crate::update::impact::Disposition::Resumes,
                reason: String::new(),
                interrupted_subagents: Vec::new(),
            }],
            blockers: Vec::new(),
        };
        let settled = serde_json::json!({
            "version":2, "job":job, "request_identity":hash(&serde_json::to_vec(&reviewed).unwrap()),
            "job_request_hash":hash(&request), "reviewed":reviewed, "source":proof("source"),
            "checkpoint_hash":checkpoint_hash, "census":census,
            "adopted_terminals":[],
            "adopted_chats":if case == SettledCase::MissingAdopted { vec![source.id] } else { vec![] },
            "adopted_chat_descriptors":if case == SettledCase::MissingAdopted { vec![latest_saved.clone()] } else { vec![] },
            "paused_terminals":[],
            "paused_chats":if case == SettledCase::MissingAdopted { vec![] } else { vec![source.id] },
            "settled":true, "successor":proof("first-recovery"),
            "target":daemon_binary, "target_hash":hash(&fs::read(&daemon_binary).unwrap()),
        });
        let outcome = store
            .path
            .with_file_name(format!("daemon-upgrade.recovery.{checkpoint_hash}.json"));
        let write_outcome = |value: &serde_json::Value| {
            fs::write(&outcome, serde_json::to_vec(value).unwrap()).unwrap();
            fs::set_permissions(&outcome, fs::Permissions::from_mode(0o600)).unwrap();
        };
        for changed in [
            "unsettled",
            "hash",
            "source",
            "inventory",
            "request",
            "target",
        ] {
            let mut invalid = settled.clone();
            match changed {
                "unsettled" => invalid["settled"] = false.into(),
                "hash" => invalid["checkpoint_hash"] = "0".repeat(64).into(),
                "source" => invalid["source"]["host_incarnation"] = "foreign".into(),
                "inventory" => {
                    invalid["adopted_chats"] = serde_json::json!([]);
                    invalid["paused_chats"] = serde_json::json!([]);
                }
                "request" => invalid["job_request_hash"] = "0".repeat(64).into(),
                "target" => invalid["target_hash"] = "0".repeat(64).into(),
                _ => unreachable!(),
            }
            write_outcome(&invalid);
            let host = Arc::new(state::HostLock::new(state::HostState::new(proof(
                "refused",
            ))));
            assert!(
                restore_settled_from(&host, &store.path, &daemon_binary).is_err(),
                "{changed}"
            );
            assert!(host.lock().unwrap().terminals.is_empty());
            assert!(host.lock().unwrap().chats.is_empty());
            assert!(!sentinel.exists());
            assert_eq!(fs::read(&store.path).unwrap(), checkpoint);
        }
        write_outcome(&settled);
        let mut retained = state::HostState::new(proof("retained"));
        retained.retain_startup_recovery("An original owner is still unresolved".into());
        let retained = Arc::new(state::HostLock::new(retained));
        let outcome_before = fs::read(&outcome).unwrap();
        assert!(restore_settled_from(&retained, &store.path, &daemon_binary).is_err());
        assert_eq!(fs::read(&store.path).unwrap(), checkpoint);
        assert_eq!(fs::read(&outcome).unwrap(), outcome_before);
        assert!(matches!(
            retained.lock().unwrap().startup_state(),
            crate::terminal_ipc::StartupState::Retained { .. }
        ));
        assert!(!sentinel.exists());
        let mut restoring = state::HostState::new(proof("restoring"));
        restoring.await_startup_for_test(TerminalId::new());
        let restoring = Arc::new(state::HostLock::new(restoring));
        assert!(restore_settled_from(&restoring, &store.path, &daemon_binary).is_err());
        assert_eq!(fs::read(&store.path).unwrap(), checkpoint);
        assert_eq!(fs::read(&outcome).unwrap(), outcome_before);
        assert!(matches!(
            restoring.lock().unwrap().startup_state(),
            crate::terminal_ipc::StartupState::Restoring { .. }
        ));
        assert!(!sentinel.exists());
        let requested = latest_saved;
        let mut initial = state::HostState::new(proof("cold-start"));
        initial.install_model(super::super::model::Registry::open().unwrap());
        let host = Arc::new(state::HostLock::new(initial));
        assert!(restore_settled_from(&host, &store.path, &daemon_binary).unwrap());
        assert!(matches!(
            host.lock().unwrap().startup_state(),
            crate::terminal_ipc::StartupState::Ready
        ));
        assert!(host.lock().unwrap().chats.is_empty());
        assert_eq!(fs::read(&store.path).unwrap(), checkpoint);
        assert_eq!(fs::read(&outcome).unwrap(), outcome_before);
        if case == SettledCase::MissingAdopted {
            assert!(
                crate::upgrade::floor::load()
                    .unwrap()
                    .resume_source(&requested)
                    .is_none()
            );
            assert!(crate::upgrade::floor::snapshot(&history_source).is_err());
            let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
            let server_host = host.clone();
            let server =
                std::thread::spawn(move || super::super::client::serve(server, 1, server_host));
            let failure = match chat::client::ClientHandle::fixture_restore(client, requested) {
                Err(failure) => failure,
                Ok(_) => panic!("a missing adopted owner cannot manufacture a paused page"),
            };
            assert_eq!(failure.recovery, chat::Recovery::Retry);
            server.join().unwrap();
            assert!(crate::upgrade::floor::load().unwrap().records.is_empty());
            assert!(crate::upgrade::floor::snapshot(&history_source).is_err());
            assert_eq!(fs::read(&store.path).unwrap(), checkpoint);
            assert_eq!(fs::read(&outcome).unwrap(), outcome_before);
            assert!(!sentinel.exists());
            return;
        }
        assert_eq!(
            crate::upgrade::floor::load()
                .unwrap()
                .resume_source(&requested),
            Some(&history_source)
        );
        assert_eq!(
            crate::upgrade::floor::snapshot(&history_source)
                .unwrap()
                .source,
            history_source
        );

        // A different confirmed thread cannot borrow the captured page even
        // when all other fields match the exact captured descriptor A.
        let mut wrong_thread = requested.clone();
        wrong_thread.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(
            crate::upgrade::floor::load()
                .unwrap()
                .resume_source(&wrong_thread)
                .is_none()
        );
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let server_host = host.clone();
        let server =
            std::thread::spawn(move || super::super::client::serve(server, 2, server_host));
        let failure = match chat::client::ClientHandle::fixture_restore(client, wrong_thread) {
            Err(failure) => failure,
            Ok(_) => panic!("a different thread must not display the captured page"),
        };
        assert_eq!(failure.recovery, chat::Recovery::Retry);
        server.join().unwrap();
        assert!(host.lock().unwrap().chats.is_empty());
        assert!(!sentinel.exists());

        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let server_host = host.clone();
        let server =
            std::thread::spawn(move || super::super::client::serve(server, 1, server_host));
        let client =
            chat::client::ClientHandle::fixture_restore(client, requested.clone()).unwrap();
        assert_eq!(
            client.session().launch.session_id,
            history_source.launch.session_id
        );
        assert_eq!(client.session().proof, Some(proof("cold-start")));
        let handle = host.lock().unwrap().chats[&source.id].handle.clone();
        let page = handle.snapshot(None).unwrap();
        assert!(page.exited && page.read_only_reason.is_some());
        assert!(page.items.iter().any(|item| matches!(&item.content, chat::ItemContent::Text { text, .. } if text == "The captured page survived")));
        assert!(handle.is_read_only_history() && !handle.owns_work());
        assert!(!sentinel.exists());
        let archived = if matches!(
            case,
            SettledCase::OrdinaryClose | SettledCase::CapturedIdentity
        ) {
            let verified = recovery::classify_retained_prepared(&store.path, &daemon_binary)
                .unwrap()
                .unwrap();
            verified
                .archive_completed(&proof("cold-start"), &[], &[], &[], &[source.id])
                .unwrap();
            assert!(!store.path.exists());
            let archived = recovery::archived_proof(&store.path, &daemon_binary)
                .unwrap()
                .unwrap();
            assert_eq!(fs::read(&archived.archive_path).unwrap(), checkpoint);
            Some(archived)
        } else {
            None
        };
        assert_eq!(fs::read(&outcome).unwrap(), outcome_before);
        let registry = super::super::model::Registry::open().unwrap();
        assert_eq!(
            registry.agent_for_session(source.id).unwrap(),
            Some(original_agent.clone())
        );
        let ledger = crate::upgrade::floor::load().unwrap();
        assert_eq!(ledger.records[0].presentations[0].pane, pane);
        assert!(ledger.records[0].presentations[0].restored);
        drop(registry);
        handle.close().unwrap();
        drop(handle);
        drop(client);
        server.join().unwrap();
        drop(host);

        // This is a genuine user close after seeing the paused history, using
        // the production transaction that retires its canonical managed pane.
        let mut registry = super::super::model::Registry::open().unwrap();
        let closed = registry
            .close_agent(&original_agent.origin, original_agent.id)
            .unwrap();
        assert!(closed.closed);
        let after_close = registry.model().unwrap();
        let closed_view = after_close
            .views
            .iter()
            .find(|view| view.scope == original_agent.placement)
            .unwrap();
        assert!(closed_view.retired.contains(&pane.id));
        assert!(
            after_close
                .views
                .iter()
                .all(|view| view.panes.iter().all(|saved| saved.id != pane.id))
        );
        drop(registry);
        // The history exists until ordinary startup's production pruning pass.
        assert!(crate::upgrade::floor::snapshot(&history_source).is_ok());
        if case == SettledCase::CanonicalClose {
            // Production pruning removes the genuinely closed paused page.
            // The still-canonical settled Prepared file must never recreate it.
            model::floor::restore_presentations().unwrap();
            assert!(crate::upgrade::floor::load().unwrap().records.is_empty());
            assert!(crate::upgrade::floor::snapshot(&history_source).is_err());
            let mut initial = state::HostState::new(proof("closed-canonical-restart"));
            initial.install_model(super::super::model::Registry::open().unwrap());
            let host = Arc::new(state::HostLock::new(initial));
            assert!(restore_settled_from(&host, &store.path, &daemon_binary).unwrap());
            assert!(matches!(
                host.lock().unwrap().startup_state(),
                crate::terminal_ipc::StartupState::Ready
            ));
            assert!(host.lock().unwrap().chats.is_empty());
            let registry = super::super::model::Registry::open().unwrap();
            assert_eq!(registry.agent(original_agent.id).unwrap(), closed);
            let model = registry.model().unwrap();
            assert!(
                model
                    .views
                    .iter()
                    .all(|view| view.panes.iter().all(|saved| saved.id != pane.id))
            );
            assert!(
                model
                    .views
                    .iter()
                    .find(|view| view.scope == original_agent.placement)
                    .unwrap()
                    .retired
                    .contains(&pane.id)
            );
            assert!(crate::upgrade::floor::load().unwrap().records.is_empty());
            assert!(
                crate::upgrade::floor::load()
                    .unwrap()
                    .resume_source(&requested)
                    .is_none()
            );
            assert!(crate::upgrade::floor::snapshot(&history_source).is_err());
            assert_eq!(fs::read(&store.path).unwrap(), checkpoint);
            assert_eq!(fs::read(&outcome).unwrap(), outcome_before);
            assert!(!sentinel.exists());
            return;
        }
        let archived = archived.unwrap();
        let archived_checkpoint = fs::read(&archived.archive_path).unwrap();
        let archived_retirement = fs::read(&archived.retirement_path).unwrap();
        let ready_outcome = store
            .path
            .with_file_name("daemon-upgrade.ready-outcome.json");
        let active_pointer = store
            .path
            .with_file_name("daemon-upgrade.retirement-active.json");
        let active_before = fs::read(&active_pointer).unwrap();
        for (previous, epoch, stale_pointer) in [
            ("cold-start", "closed-restart-one", "future-version"),
            (
                "closed-restart-one",
                "closed-restart-two",
                "missing-payload",
            ),
        ] {
            // Only the optional route is stale. The original outcome,
            // retirement and archived checkpoint remain byte-identical.
            let mut stale: serde_json::Value = serde_json::from_slice(&active_before).unwrap();
            stale["target"] = serde_json::to_value(std::env::current_exe().unwrap()).unwrap();
            match stale_pointer {
                "future-version" => stale["version"] = serde_json::json!(999),
                "missing-payload" => stale["retirement_hash"] = serde_json::json!("0".repeat(64)),
                _ => unreachable!(),
            }
            fs::write(&active_pointer, serde_json::to_vec(&stale).unwrap()).unwrap();
            let mut ready = crate::upgrade::handoff::Checkpoint::new(
                proof(previous),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            );
            ready.phase = crate::upgrade::handoff::Phase::Ready;
            store.save(&ready).unwrap();
            let mut initial = state::HostState::new(proof(epoch));
            initial.install_model(super::super::model::Registry::open().unwrap());
            let host = Arc::new(state::HostLock::new(initial));
            restore_ordinary(&host).unwrap();
            assert!(matches!(
                host.lock().unwrap().startup_state(),
                crate::terminal_ipc::StartupState::Ready
            ));
            assert!(host.lock().unwrap().chats.is_empty());
            assert!(host.lock().unwrap().terminals.is_empty());
            assert!(
                !store.path.exists(),
                "ordinary Ready restoration consumes its own checkpoint"
            );
            let published: serde_json::Value =
                serde_json::from_slice(&fs::read(&ready_outcome).unwrap()).unwrap();
            assert_eq!(
                published["serving"],
                serde_json::to_value(proof(epoch)).unwrap()
            );
            assert_eq!(published["partitions"], serde_json::json!([]));
            let registry = super::super::model::Registry::open().unwrap();
            assert_eq!(registry.agent(original_agent.id).unwrap(), closed);
            assert!(registry.agent_for_session(source.id).unwrap().is_none());
            let model = registry.model().unwrap();
            assert!(
                model
                    .views
                    .iter()
                    .all(|view| view.panes.iter().all(|saved| saved.id != pane.id))
            );
            assert!(
                model
                    .views
                    .iter()
                    .find(|view| view.scope == original_agent.placement)
                    .unwrap()
                    .retired
                    .contains(&pane.id)
            );
            assert!(crate::upgrade::floor::load().unwrap().records.is_empty());
            assert!(
                crate::upgrade::floor::load()
                    .unwrap()
                    .resume_source(&requested)
                    .is_none()
            );
            assert!(
                crate::upgrade::floor::snapshot(&history_source).is_err(),
                "closed paused page must not be recreated from archived metadata"
            );
            assert_eq!(
                fs::read(&archived.archive_path).unwrap(),
                archived_checkpoint
            );
            assert_eq!(
                fs::read(&archived.retirement_path).unwrap(),
                archived_retirement
            );
            assert_eq!(fs::read(&outcome).unwrap(), outcome_before);
            assert!(!sentinel.exists());
            fs::write(&active_pointer, &active_before).unwrap();
            assert_eq!(fs::read(&active_pointer).unwrap(), active_before);
        }
    }

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
        let mut source = Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: LaunchRecipe {
                    program: "/must-not-execute-a-recovery-provider".into(),
                    arguments: vec!["--remote".into(), "unix://".into()],
                    working_directory: root.join("project"),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: None,
            },
            proof: Some(proof("old")),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        assert!(source.claim_owned_codex_listener(&root.join("run"), &proof("old")));
        source.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
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
        let (registry, pane) = seed_managed_pane(
            &root.join("registry.sqlite3"),
            &root.join("project"),
            &alias,
        );
        let agent = registry.agent_for_session(source.id).unwrap().unwrap();
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
        // Retained-owner adoption precedes paused ledger publication during
        // real recovery. Its canonical pane lineage check reads this same
        // index before the newly paused conversations have been recorded.
        let mut adopted = source.clone();
        adopted.proof = Some(proof("new"));
        let _ = chat::is_upgrade_recovery(&source, &adopted);
        let before = crate::upgrade::floor::load().unwrap();
        assert!(before.resume_source(&alias).is_none());
        paused.save().unwrap();
        assert!(before.resume_source(&alias).is_none());
        assert_eq!(
            crate::upgrade::floor::load().unwrap().resume_source(&alias),
            Some(&source)
        );
        let ledger = crate::upgrade::floor::load().unwrap();
        assert_eq!(ledger.resume_source(&alias), Some(&source));
        assert_eq!(ledger.records[0].presentations[0].pane, pane);
        let mut initial = state::HostState::new(proof("new"));
        initial.install_model(registry);
        let host = Arc::new(state::HostLock::new(initial));
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let server_host = host.clone();
        let server = std::thread::spawn(move || {
            super::super::client::serve(server, 1, server_host);
        });
        let client = match chat::client::ClientHandle::fixture_restore(client, alias.clone()) {
            Ok(client) => client,
            Err(failure) => {
                host.lock().unwrap().register_client(2);
                let underlying = super::super::chat::upgrade_resolution::resolve(&host, 2, &alias);
                panic!(
                    "real client restore failed: {failure:?}; direct resolution: {underlying:?}"
                );
            }
        };
        assert_eq!(client.session().launch.session_id, source.launch.session_id);
        let resolved = client.session();
        let reader = crate::workspace_model::views::Pane {
            kind: crate::model::CardKind::Chat(resolved.clone()),
            ..pane.clone()
        };
        let mut model = crate::workspace_model::WorkspaceModel {
            agents: vec![agent.clone()],
            views: vec![crate::workspace_model::views::View {
                scope: agent.placement.clone(),
                revision: 2,
                panes: vec![reader.clone()],
                retired: Vec::new(),
            }],
            ..Default::default()
        };
        let reads = crate::upgrade::floor::index_reads_for_test();
        let unproved = crate::workspace_model::views::ManagedDisplayIndex::new(&model);
        assert!(!unproved.permits_projection(&agent, &reader));
        assert!(unproved.resolve(&agent).is_none());
        let index =
            crate::workspace_model::views::ManagedDisplayIndex::with_history(&model, Some(&ledger));
        for _ in 0..100 {
            assert!(index.permits_projection(&agent, &reader));
            assert_eq!(index.resolve(&agent).unwrap().pane, Some(reader.clone()));
        }
        assert_eq!(crate::upgrade::floor::index_reads_for_test(), reads);
        for changed in [
            Session {
                proof: None,
                ..resolved.clone()
            },
            Session {
                launch: chat::Launch {
                    session_id: Some(uuid::Uuid::new_v4().to_string()),
                    ..resolved.launch.clone()
                },
                ..resolved.clone()
            },
            Session {
                launch: chat::Launch {
                    recipe: LaunchRecipe {
                        program: "/different-provider".into(),
                        ..resolved.launch.recipe.clone()
                    },
                    ..resolved.launch.clone()
                },
                ..resolved.clone()
            },
        ] {
            let changed = crate::workspace_model::views::Pane {
                kind: crate::model::CardKind::Chat(changed),
                ..reader.clone()
            };
            assert!(!index.permits_projection(&agent, &changed));
        }
        let mut uncaptured = agent.clone();
        if let Some(crate::agent_execution::HostedSession::Chat(session)) = &mut uncaptured.session
        {
            session.launch.recipe.program = "/uncaptured-provider".into();
        }
        assert!(!index.permits_projection(&uncaptured, &reader));
        model.views.push(crate::workspace_model::views::View {
            scope: crate::workspace_model::views::Scope::Channel(crate::model::ChannelId::new()),
            revision: 1,
            panes: vec![reader.clone()],
            retired: Vec::new(),
        });
        let ambiguous =
            crate::workspace_model::views::ManagedDisplayIndex::with_history(&model, Some(&ledger));
        assert!(!ambiguous.permits_projection(&agent, &reader));
        assert!(ambiguous.resolve(&agent).is_none());
        assert_eq!(
            agent.session,
            Some(crate::agent_execution::HostedSession::Chat(alias.clone()))
        );
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
        drop(client);
        server.join().unwrap();
    }
}
