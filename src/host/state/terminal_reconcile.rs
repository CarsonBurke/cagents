//! Explicit live-holder adoption never authorizes launching another process.
use super::*;

fn unavailable(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, false)
}
/// The host publishes a Live proof and its deterministic launch observation;
/// holders retain DetachedSameBoot. No recipe/account/listener fields differ.
fn issued_source(saved: &TerminalSession, requested: &TerminalSession) -> bool {
    let Some(proof) = saved.continuity.live_proof().filter(|proof| proof.is_complete()) else { return false; };
    if requested.continuity.live_proof().as_ref() != Some(&proof) { return false; }
    let mut candidate = saved.clone();
    candidate.continuity = requested.continuity.clone();
    if &candidate == requested { return true; }
    candidate.resume_hint = crate::model::agent_recovery::observed_hint_from_launch(&candidate.launch);
    &candidate == requested
}

pub(in crate::host) fn reconcile(
    host: &SharedHost, client: ClientId, requested: &TerminalSession,
) -> Result<TerminalDescriptor, ProtocolError> {
    let (proof, existing) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down || state.upgrade.is_some() || state.spawning.contains(&requested.id)
            || state.chat_spawning.contains_key(&requested.id) || state.chat_handoffs.contains_key(&requested.id) {
            return Err(unavailable("Terminal ownership is changing; retry after that operation settles"));
        }
        let existing = if let Some(record) = state.terminals.get(&requested.id) {
            // Existing native/console workers retain their established attach
            // route. This never adopts their missing holder as an ordinary PTY.
            let mut current = record.session.clone();
            let proof = state.proof();
            current.continuity = TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id.clone(), boot_id: proof.boot_id.clone(), host_incarnation: proof.host_incarnation.clone(),
            };
            if !issued_source(&current, requested)
                && !record.upgrade_sources.iter().any(|source| issued_source(source, requested)) {
                return Err(unavailable("The hosted terminal does not match this exact saved descriptor; no process was replaced"));
            }
            Some((record.handle.clone(), record.upgrade_sources.clone(), record.authority_rebind_pending))
        } else { None };
        if existing.is_none() && (requested.owned_codex_listener.is_some() || state.chats.contains_key(&requested.id)) {
            return Err(unavailable("This terminal belongs to a native conversation; reconnect through its owning conversation"));
        }
        if requested.continuity.live_proof().is_none_or(|proof| !proof.is_complete() || proof.boot_id != state.proof().boot_id) {
            return Err(unavailable("Reconnecting requires this boot's complete saved terminal proof; no command was restarted"));
        }
        (state.proof().clone(), existing)
    };
    if let Some((expected, previous, pending)) = existing {
        if pending { restore_terminal_execution_authority(host, requested, &previous).map_err(unavailable)?; }
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.proof() != &proof || !state.terminals.get(&requested.id).is_some_and(|record| record.handle.same_worker(&expected)) {
            return Err(unavailable("The terminal worker changed during reconciliation; no replacement was attached"));
        }
        return state.attach(client, requested.id);
    }
    let socket = crate::terminal::holder::socket(&crate::terminal::holder::directory().map_err(unavailable)?, requested.id);
    let record = crate::terminal::holder::evidence(&socket).map_err(unavailable)?
        .ok_or_else(|| not_found(requested.id))?;
    let (recovery, screen_lost) = crate::terminal::inspect_holder(&socket, &record, &proof).map_err(unavailable)?;
    if !std::iter::once(&recovery.source).chain(recovery.previous_sources.iter()).any(|source| issued_source(source, requested)) {
        return Err(unavailable("The original holder does not recognize this exact saved recipe and proof; its processes were preserved"));
    }
    let source = recovery.source.clone();
    let previous = recovery.previous_sources.clone();
    let relaunches = recovery.relaunches.clone();
    // Adoption uses the holder's actual launch directory descriptor and PTY.
    // A deleted launch pathname never permits a substitute cwd or new shell.
    let mut session = source.clone();
    session.continuity = TerminalContinuity::RelaunchAvailable { recipe: session.launch.clone(), last_exit: None };
    start_terminal_view(host, client, session, recovery.options.clone(), recovery.execution_environment.clone(), false,
        Some(socket), Some(recovery))?;
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.adopt_upgrade_lineage(&source, &previous, &relaunches)?;
        let terminal = state.terminals.get_mut(&requested.id).ok_or_else(|| not_found(requested.id))?;
        terminal.screen_lost_after_crash = screen_lost;
        terminal.session.resume_hint = source.resume_hint.clone();
    }
    restore_terminal_execution_authority(host, &source, &previous).map_err(unavailable)?;
    host.lock().unwrap_or_else(|p| p.into_inner()).attach(client, requested.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconciliation_binds_recipe_account_listener_identity_and_original_complete_proof() {
        let proof = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("terminal-original"),
            boot_id: crate::model::BootId::new("terminal-boot"),
            host_incarnation: crate::model::HostIncarnation::new("terminal-incarnation"),
        };
        let saved = TerminalSession {
            id: TerminalId::new(), launch: crate::model::LaunchRecipe {
                program: "/bin/sh".into(), arguments: vec![], working_directory: "/var/tmp".into(),
                environment: BTreeMap::from([("ACCOUNT_HOME".into(), "/private/account".into())]), unset_environment: Default::default(),
            }, continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id.clone(), boot_id: proof.boot_id.clone(), host_incarnation: proof.host_incarnation.clone(),
            }, transcript_path: None, resume_hint: None, owned_codex_listener: None, recovery_origin: None, predecessor: None,
        };
        let mut requested = saved.clone();
        requested.continuity = TerminalContinuity::Live { host_session_id: proof.host_session_id.clone(), boot_id: proof.boot_id.clone(), host_incarnation: proof.host_incarnation.clone() };
        assert!(issued_source(&saved, &requested));
        for mutation in 0..6 {
            let mut forged = requested.clone();
            match mutation {
                0 => forged.launch.program = "/unrelated/program".into(),
                1 => forged.launch.arguments.push("unissued".into()),
                2 => forged.launch.working_directory = "/unrelated/cwd".into(),
                3 => { forged.launch.environment.insert("ACCOUNT_HOME".into(), "/unrelated/account".into()); },
                4 => forged.id = TerminalId::new(),
                _ => forged.continuity = TerminalContinuity::Live { host_session_id: proof.host_session_id.clone(), boot_id: proof.boot_id.clone(), host_incarnation: crate::model::HostIncarnation::new("unissued-proof") },
            }
            assert!(!issued_source(&saved, &forged), "accepted mutation {mutation}");
        }
        requested.continuity = TerminalContinuity::RelaunchAvailable { recipe: saved.launch.clone(), last_exit: None };
        assert!(!issued_source(&saved, &requested), "a recipe is not live process authority");
    }
}

#[cfg(test)]
mod authority_regression {
    use super::*;
    #[test]
    fn sql_failure_keeps_original_worker_unavailable_until_a_later_explicit_retry_rebinds() {
        use crate::{agent_execution::HostedSession, model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe}, terminal::TerminalConfig};
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let path = directory.path().join("registry.sqlite3");
        let proof = |generation: &str| HostSessionProof { host_session_id: HostSessionId::new("rebind-test"), boot_id: BootId::new("fixture-boot"), host_incarnation: HostIncarnation::new(generation) };
        let old = proof("old"); let next = proof("next");
        let source = TerminalSession { id: TerminalId::new(), launch: LaunchRecipe {
            program: "/bin/sh".into(), arguments: vec!["-c".into(), "read -r value".into()], working_directory: directory.path().to_owned(), environment: Default::default(), unset_environment: Default::default(),
        }, continuity: TerminalContinuity::Live { host_session_id: old.host_session_id, boot_id: old.boot_id, host_incarnation: old.host_incarnation },
            transcript_path: None, resume_hint: None, owned_codex_listener: None, recovery_origin: None, predecessor: None };
        let mut registry = super::super::super::model::Registry::at(&path).unwrap();
        let token = registry.register_agent_authority(&HostedSession::Terminal(source.clone())).unwrap();
        let mut state = HostState::new(next.clone());
        state.install_model(registry);
        let handle = TerminalHandle::spawn(source.clone(), TerminalConfig::default(), |_| {}).unwrap();
        struct Cleanup(TerminalHandle);
        impl Drop for Cleanup { fn drop(&mut self) { let _ = self.0.shutdown(); } }
        let _cleanup = Cleanup(handle.clone());
        state.finish_spawn(0, source.clone(), handle.clone(), false).unwrap();
        let record = state.terminals.get_mut(&source.id).unwrap();
        record.upgrade_sources.push(source.clone());
        record.authority_rebind_pending = true;
        let host = Arc::new(HostLock::new(state));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TRIGGER reject_rebind BEFORE UPDATE ON agent_authorities BEGIN SELECT RAISE(ABORT,'fixture authority write failure'); END;").unwrap();
        assert!(reconcile(&host, 1, &source).is_err());
        {
            let mut state = host.lock().unwrap();
            assert!(state.terminals[&source.id].handle.same_worker(&handle));
            assert!(state.terminals[&source.id].authority_rebind_pending);
            assert!(matches!(state.descriptor(1, source.id).unwrap().session.continuity, TerminalContinuity::Failed { .. }));
            assert!(state.attach(1, source.id).is_err(), "ordinary Attach cannot bypass failed durable binding");
            assert_eq!(state.execution_registry.as_ref().unwrap().lock().unwrap().agent_authority_source(&token).unwrap(), Some(HostedSession::Terminal(source.clone())));
        }
        db.execute_batch("DROP TRIGGER reject_rebind;").unwrap();
        let restored = reconcile(&host, 1, &source).unwrap();
        assert_eq!(restored.session.continuity.live_proof(), Some(next.clone()));
        let state = host.lock().unwrap();
        assert!(!state.terminals[&source.id].authority_rebind_pending);
        assert!(state.terminals[&source.id].handle.same_worker(&handle));
        assert_eq!(state.execution_registry.as_ref().unwrap().lock().unwrap().agent_authority_source(&token).unwrap(), Some(HostedSession::Terminal(restored.session)));
    }
}
