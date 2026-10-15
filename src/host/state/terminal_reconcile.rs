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
    // Foreground-agent hints are observations of the same PTY, excluded from
    // the existing cleanup digest too. They do not choose a resume recipe here.
    candidate.resume_hint = requested.resume_hint.clone();
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
    // Durable Chat ownership is not an ordinary shell recipe. A console may
    // omit owned_codex_listener, so authenticate its stored association rather
    // than infer ownership from executable names or the missing live actor.
    let native_owned = match super::super::agent_execution::registry(host) {
        Ok(registry) => registry.lock().unwrap_or_else(|p| p.into_inner())
            .retained_agent_sources(requested.id).map_err(unavailable)?.iter()
            .any(|source| matches!(source, crate::agent_execution::HostedSession::Chat(_))),
        Err(error) if existing.is_none() => return Err(unavailable(error)),
        Err(_) => false, // Existing exact workers retain their known host owner.
    };
    if native_owned && (existing.is_none() || !host.lock().unwrap_or_else(|p| p.into_inner()).native_terminal_binding_known(requested.id)) {
        return Err(unavailable("This console belongs to a native conversation; reconnect its exact native actor and terminal pair first. Its original holder was preserved"));
    }
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
        requested.resume_hint = Some("observed-codex-thread-inside-original-shell".into());
        assert!(issued_source(&saved, &requested), "a foreground observation does not change the original PTY owner");
        for mutation in 0..8 {
            let mut forged = requested.clone();
            match mutation {
                0 => forged.launch.program = "/unrelated/program".into(),
                1 => forged.launch.arguments.push("unissued".into()),
                2 => forged.launch.working_directory = "/unrelated/cwd".into(),
                3 => { forged.launch.environment.insert("ACCOUNT_HOME".into(), "/unrelated/account".into()); },
                4 => forged.id = TerminalId::new(),
                5 => { forged.launch.unset_environment.insert("ACCOUNT_HOME".into()); },
                6 => forged.owned_codex_listener = Some(crate::chat::OwnedCodexListener { socket: "/unrelated/listener".into(), allocator: proof.clone() }),
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
    fn durable_native_console_cannot_become_an_ordinary_ready_worker_before_its_actor() {
        use crate::{agent_execution::HostedSession, model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe}, terminal::TerminalConfig};
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let proof = |generation: &str| HostSessionProof { host_session_id: HostSessionId::new("console-owner"),
            boot_id: BootId::new("console-boot"), host_incarnation: HostIncarnation::new(generation) };
        let old = proof("old"); let next = proof("next");
        let source = TerminalSession { id: TerminalId::new(), launch: LaunchRecipe { program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "read -r value".into()], working_directory: directory.path().into(),
            environment: Default::default(), unset_environment: Default::default() },
            continuity: TerminalContinuity::Live { host_session_id: old.host_session_id.clone(), boot_id: old.boot_id.clone(), host_incarnation: old.host_incarnation.clone() },
            transcript_path: None, resume_hint: None, owned_codex_listener: None, recovery_origin: None, predecessor: None };
        let owner = crate::chat::Session { id: source.id, launch: crate::chat::Launch { provider: crate::agent::Provider::Claude,
            recipe: source.launch.clone(), session_id: Some("native-console-owner".into()) }, proof: Some(old), terminal: Some(source.clone()),
            native_continuity: Default::default(), owned_codex_listener: None };
        let mut registry = super::super::super::model::Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
        let token = registry.register_agent_authority(&HostedSession::Chat(owner.clone())).unwrap();
        let mut state = HostState::new(next.clone()); state.install_model(registry);
        let host = Arc::new(HostLock::new(state));
        assert!(reconcile(&host, 1, &source).unwrap_err().message.contains("native conversation"));
        assert!(host.lock().unwrap().terminals.is_empty(), "explicit retry must refuse before installing an ordinary worker");
        // Startup can authenticate the terminal half first, but it remains
        // provisional until native recovery authenticates/installs its actor.
        let mut current = source.clone();
        current.continuity = TerminalContinuity::RelaunchAvailable { recipe: current.launch.clone(), last_exit: None };
        let handle = TerminalHandle::spawn(current.clone(), TerminalConfig::default(), |_| {}).unwrap();
        struct Cleanup(TerminalHandle);
        impl Drop for Cleanup { fn drop(&mut self) { let _ = self.0.shutdown(); } }
        let _cleanup = Cleanup(handle.clone());
        {
            let mut state = host.lock().unwrap(); state.finish_spawn(0, current, handle.clone(), false).unwrap();
            let record = state.terminals.get_mut(&source.id).unwrap(); record.upgrade_sources.push(source.clone()); record.authority_rebind_pending = true;
        }
        assert!(restore_terminal_execution_authority(&host, &source, &[]).is_err());
        assert!(reconcile(&host, 1, &source).is_err());
        let mut state = host.lock().unwrap();
        assert!(!state.confirm_native_terminal_binding(source.id));
        assert!(state.terminals[&source.id].authority_rebind_pending);
        assert!(state.terminals[&source.id].handle.same_worker(&handle));
        assert!(matches!(state.descriptor(1, source.id).unwrap().session.continuity, TerminalContinuity::Failed { .. }));
        assert!(state.attach(1, source.id).is_err());
        assert_eq!(state.execution_registry.as_ref().unwrap().lock().unwrap().agent_authority_source(&token).unwrap(), Some(HostedSession::Chat(owner.clone())));
        drop(state);
        // Typed native recovery commits the exact Chat successor first, then
        // installs its actor and paired worker under the host lock.
        struct Quiet;
        impl crate::chat::protocol::Adapter for Quiet {
            fn initialize(&mut self) -> Result<crate::chat::protocol::Step, String> { Ok(Default::default()) }
            fn command(&mut self, _: crate::chat::Command) -> Result<crate::chat::protocol::Step, String> { Ok(Default::default()) }
            fn receive(&mut self, _: serde_json::Value) -> Result<crate::chat::protocol::Step, String> { Ok(Default::default()) }
        }
        let mut actor_recipe = source.launch.clone(); actor_recipe.program = "/bin/cat".into(); actor_recipe.arguments.clear();
        let spec = crate::chat::protocol::ProcessSpec::from(&actor_recipe);
        let mut initial = crate::chat::protocol::Step::default();
        let store = crate::chat::runtime::preflight(&spec, &mut initial).unwrap();
        let actor = crate::chat::runtime::Handle::start(spec, Box::new(Quiet), initial, store, Some("native-console-owner".into()), |_| {}).unwrap();
        struct ActorCleanup(crate::chat::runtime::Handle);
        impl Drop for ActorCleanup { fn drop(&mut self) { let _ = self.0.close(); } }
        let _actor_cleanup = ActorCleanup(actor.clone());
        let mut successor = owner.clone(); successor.proof = Some(next.clone());
        let mut pair = source.clone(); pair.continuity = TerminalContinuity::Live { host_session_id: next.host_session_id,
            boot_id: next.boot_id, host_incarnation: next.host_incarnation };
        successor.terminal = Some(pair);
        {
            let mut state = host.lock().unwrap();
            state.execution_registry.as_ref().unwrap().lock().unwrap()
                .adopt_authenticated_agent_session(&[HostedSession::Chat(owner)], &HostedSession::Chat(successor.clone())).unwrap();
            state.chats.insert(source.id, super::super::super::chat::Record {
                execution_instance: uuid::Uuid::new_v4(), console_recipe: Some(source.launch.clone()), paired_terminal: Some(handle.clone()),
                session: successor, handle: actor, source_process: None, subscribers: Default::default(), observations: Default::default(),
                completion_observed: false, returning_terminal: false,
            });
            assert!(matches!(state.terminals[&source.id].session.continuity, TerminalContinuity::RelaunchAvailable { .. }));
            assert!(state.confirm_native_terminal_binding(source.id), "the private worker projection must not reject raw unhosted continuity");
        }
        let recovered = reconcile(&host, 1, &source).unwrap();
        assert!(recovered.session.continuity.live_proof().is_some());
        assert!(host.lock().unwrap().terminals[&source.id].handle.same_worker(&handle));
        handle.shutdown().unwrap();
        {
            let mut state = host.lock().unwrap();
            assert!(state.native_terminal_binding_known(source.id), "exact pair identity survives PTY completion");
            assert!(state.native_terminal_projection(source.id).is_err(), "completed workers cannot become live native recovery");
            assert!(!state.confirm_native_terminal_binding(source.id));
        }
        let request = |allow_active| crate::terminal_ipc::ClientRequest::ReconcileTerminalClose { session: source.clone(), allow_active };
        assert!(matches!(super::super::super::client::process_request(&host, 1, request(false)).unwrap().0,
            crate::terminal_ipc::ServerResponse::TerminalCloseChecked { outcome: crate::terminal_ipc::TerminalCloseResult::Active, .. }),
            "a completed console still needs consent for its live native actor");
        assert!(matches!(super::super::super::client::process_request(&host, 1, request(true)).unwrap().0,
            crate::terminal_ipc::ServerResponse::TerminalCloseChecked { outcome: crate::terminal_ipc::TerminalCloseResult::Closed, .. }));
        assert!(_actor_cleanup.0.is_completed());
    }

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
