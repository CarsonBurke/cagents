//! Explicit restart keeps the resident reducer's owner and its original Store.
//! Preparation is retained across ambiguous replies rather than repeated with
//! potentially changed credentials, cwd or executable configuration.
use super::*;

#[derive(Clone)]
pub(in crate::host) struct PreparedRestart {
    request: uuid::Uuid,
    settled: bool,
    failed_proved: bool,
    identity: chat::runtime::ActorIdentity,
    source: Session,
    spec: chat::protocol::ProcessSpec,
    // Issued in the exact prepared argv; only the real provider decoder may
    // confirm it. A fresh source remains session_id None in restart custody.
    expected_session_id: String,
    recovery_environment: BTreeMap<String, String>,
}

impl PreparedRestart {
    /// Allow only the host's normal projection of THIS retained request after
    /// actual decoder confirmation. The issued recipe/account/console and the
    /// original helper restart envelope remain byte-for-byte unchanged.
    fn is_confirmed_projection(&self, candidate: &Session, handle: &Handle) -> bool {
        if handle.confirmed_session_id().as_deref() != Some(self.expected_session_id.as_str()) {
            return false;
        }
        let mut projected = self.source.clone();
        projected.launch.session_id = Some(self.expected_session_id.clone());
        projected.native_continuity = handle.native_continuity();
        *candidate == projected
    }
}

/// The caller holds the exact per-chat lifecycle claim and retains `request`
/// until a positive receipt resolves it. Accepted is never a successful attach.
pub(in crate::host) fn restart_completed(
    host: &SharedHost,
    client: ClientId,
    source: &Session,
    request: uuid::Uuid,
) -> Result<Option<ServerResponse>, ProtocolError> {
    if request.is_nil() {
        return Err(error(
            "The retained restart request is missing its stable identity",
        ));
    }
    let (handle, identity, cached) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        validate_proof(source.proof.as_ref(), state.proof())?;
        let Some(record) = state.chats.get(&source.id) else {
            return Ok(None);
        };
        let Some(identity) = record.handle.actor_identity() else {
            return Ok(None);
        };
        let cached = state.native_actor_restarts.get(&source.id).cloned();
        if cached.is_none() && !record.handle.is_completed() {
            return Ok(None);
        }
        let ServerResponse::ChatAttached { session, .. } = session_snapshot(record) else {
            unreachable!()
        };
        let same_prepared_projection = cached.as_ref().is_some_and(|pending| {
            pending.request == request && pending.identity == identity
                && pending.source == *source
                && pending.is_confirmed_projection(&session, &record.handle)
        });
        if session != *source && !same_prepared_projection {
            return Err(error(
                "The retained restart changed the exact issued conversation or console",
            ));
        }
        (record.handle.clone(), identity, cached)
    };
    if cached
        .as_ref()
        .is_some_and(|cached| cached.request != request && !cached.settled)
    {
        return Err(unavailable(
            "The previous exact restart remains uncertain; retry its original request before creating another provider",
        ));
    }
    let prepared = if let Some(cached) = cached.filter(|cached| cached.request == request) {
        if cached.identity != identity || (cached.source != *source
            && !cached.is_confirmed_projection(source, &handle)) {
            return Err(error(
                "The restart receipt belongs to another exact retained owner",
            ));
        }
        cached
    } else {
        if !handle.is_completed() {
            return Err(unavailable(
                "The retained provider still owns work; no replacement was started",
            ));
        }
        check_launch_account(&source.launch)?;
        let mut launch = source.launch.clone();
        chat::protocol::align_resume_identity(&mut launch).map_err(error)?;
        let (mut spec, _) = chat::protocol::create(&launch).map_err(unavailable)?;
        let expected_session_id = match &launch.session_id {
            Some(id) => id.clone(),
            None => chat::protocol::claude::prepared_fresh_session_id(&launch, &spec.arguments)
                .map_err(unavailable)?,
        };
        if !matches!(spec.transport, chat::protocol::TransportSpec::JsonLines) {
            return Err(unavailable(
                "Retained restart requires the same Claude stream transport",
            ));
        }
        let current_directory = handle.close_observation().working_directory;
        let context = handle
            .pinned_execution_directory(&current_directory)
            .map_err(unavailable)?;
        context
            .verify_directory(&current_directory)
            .map_err(unavailable)?;
        spec.working_directory = current_directory;
        spec.execution = Some(context);
        spec.environment = handle.execution_environment().0;
        spec.unset_environment.clear();
        // The resident actor retains its original sealed mailbox and validates
        // this exact account environment again while starting the new provider.
        let account = crate::account::registry::authorize_spawn(&spec.environment, None)
            .map_err(|error| unavailable(error.to_string()))?;
        drop(account);
        let prepared = PreparedRestart {
            request,
            settled: false,
            failed_proved: false,
            identity: identity.clone(),
            source: source.clone(),
            spec,
            expected_session_id,
            recovery_environment: handle.restart_environment().0,
        };
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down
            || state.chats.get(&source.id).is_none_or(|record| {
                !record.handle.same_actor(&handle) || !record.handle.is_completed()
            })
        {
            return Err(unavailable(
                "The retained owner changed during restart preparation; no provider was started",
            ));
        }
        let live = state.chats.keys().copied().collect::<BTreeSet<_>>();
        state
            .native_actor_restarts
            .retain(|id, _| live.contains(id));
        if !state.native_actor_restarts.contains_key(&source.id)
            && state.native_actor_restarts.len() >= 128
        {
            return Err(unavailable(
                "Retained restart capacity reached; no provider was started",
            ));
        }
        state
            .native_actor_restarts
            .insert(source.id, prepared.clone());
        prepared
    };
    // Final outcomes from the crashed provider must commit before its journal
    // authorizes a new incarnation. No accepted prompt is replayed here.
    let deadline = Instant::now() + Duration::from_secs(30);
    while super::drain(host, source.id).map_err(unavailable)? {
        if Instant::now() >= deadline {
            return Err(unavailable(
                "The previous provider's final outcomes remain pending; retry this same restart",
            ));
        }
    }
    let _publication = if !prepared.settled {
        Some(super::super::source_binding::restarting(host, source.id, &handle)?)
    } else { None };
    let receipt = handle
        .restart_remote(
            request,
            prepared.spec,
            prepared.source,
            prepared.recovery_environment,
        )
        .map_err(unavailable)?;
    if receipt.id != request {
        return Err(unavailable(
            "The retained provider returned another restart identity",
        ));
    }
    match receipt.state {
        chat::runtime::RetainedRestartState::Accepted => {
            return Err(unavailable(
                "The exact restart is still pending; retry this pane to inspect the same provider receipt",
            ));
        }
        chat::runtime::RetainedRestartState::Acknowledged(Err(failure)) => {
            if matches!(
                failure.cleanup,
                chat::runtime::RetainedRestartCleanup::TeardownProved
            ) {
                let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(pending) = state
                    .native_actor_restarts
                    .get_mut(&source.id)
                    .filter(|pending| pending.request == request && pending.identity == identity)
                {
                    pending.settled = true;
                    pending.failed_proved = true;
                }
            }
            return Err(unavailable(failure.reason));
        }
        chat::runtime::RetainedRestartState::Acknowledged(Ok(_)) => {}
    }
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let pending = state
            .native_actor_restarts
            .get_mut(&source.id)
            .filter(|pending| pending.request == request && pending.identity == identity)
            .ok_or_else(|| {
                unavailable("The restart claim changed before its positive receipt was retained")
            })?;
        pending.settled = true;
    }
    handle.refresh_retained_state().map_err(unavailable)?;
    // Only a positive restart receipt permits fresh concrete helper evidence
    // to resolve Unknown. Lost/Accepted/error replies leave it fenced.
    super::super::source_binding::refresh(host, source.id, true)?;
    if handle.is_completed()
        || handle.confirmed_session_id().as_deref() != Some(prepared.expected_session_id.as_str())
        || !handle.update_activity().1
    {
        return Err(unavailable(
            "The restarted provider has not confirmed readiness; its exact request and conversation were retained",
        ));
    }
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    validate_proof(source.proof.as_ref(), state.proof())?;
    let record = state
        .chats
        .get_mut(&source.id)
        .filter(|record| {
            record.handle.same_actor(&handle) && record.execution_instance == identity.instance
        })
        .ok_or_else(|| unavailable("The retained actor changed before restart publication"))?;
    record.session.native_continuity = handle.native_continuity();
    record.completion_observed = false;
    if client != 0 {
        record.subscribers.entry(client).or_insert(false);
        record
            .observations
            .entry(client)
            .or_insert_with(|| Observation {
                after_generation: handle.observe_next_update(),
                ..Default::default()
            });
    }
    let response = session_snapshot(record);
    state.inventory_generation += 1;
    drop(state);
    handle.wake_retained_outcomes();
    Ok(Some(response))
}

/// The pair publisher may cross original None to confirmed Some only through
/// THIS same retained restart's positive receipt and canonical prepared UUID.
/// This query performs no provider I/O, creates no identity and changes no
/// source/recipe; an ordinary fresh session has no such cached authority.
pub(in crate::host) fn confirms_restart_projection(
    host: &SharedHost, source: &Session, current: &Session, handle: &Handle,
) -> Result<bool, String> {
    if source.launch.session_id.is_some() || current.launch.session_id.is_none() {
        return Ok(false);
    }
    let request = super::super::restart::request_id(host, source.id)
        .map_err(|error| error.message)?;
    let Some(identity) = handle.actor_identity() else { return Ok(false); };
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if source.proof.as_ref() != Some(state.proof())
        || state.chats.get(&source.id).is_none_or(|record| {
            !record.handle.same_actor(handle) || record.execution_instance != identity.instance
        })
    {
        return Ok(false);
    }
    Ok(state.native_actor_restarts.get(&source.id).is_some_and(|pending| {
        pending.request == request && pending.identity == identity
            && pending.source == *source && pending.settled && !pending.failed_proved
            && pending.is_confirmed_projection(current, handle)
    }))
}

/// A new user retry may prepare another request only after the helper's
/// explicit positive teardown receipt for this exact failed request.
pub(in crate::host) fn restart_failure_proved(
    host: &SharedHost,
    id: TerminalId,
    request: uuid::Uuid,
) -> bool {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    state.native_actor_restarts.get(&id).is_some_and(|pending| {
        pending.request == request
            && pending.failed_proved
            && state.chats.get(&id).is_some_and(|record| {
                record.execution_instance == pending.identity.instance
                    && record.handle.actor_identity().as_ref() == Some(&pending.identity)
            })
    })
}
