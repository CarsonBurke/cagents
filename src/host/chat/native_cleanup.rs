//! Exact, retryable cleanup when a retained provider cannot attach its history.
use super::*;
use crate::daemon::holder_registry::{self as registry, Kind};

#[derive(Clone)]
pub(in crate::host) struct NativeClose {
    source: Session,
    owner: NativeOwner,
    closed: bool,
    paired: Option<RetainedPair>,
    consented_backend: Option<crate::daemon::holder_registry::Process>,
}

impl NativeClose {
    pub(super) fn consented_backend(&self) -> Option<&registry::Process> { self.consented_backend.as_ref() }
    pub(super) fn owner_record(&self) -> &registry::Record { self.owner.record() }
    pub(super) fn paired_owner(&self) -> Option<&registry::Record> {
        self.paired.as_ref().map(|pair| &pair.holder)
    }
    pub(super) fn owns(&self, owner: &registry::Record, source: &Session) -> bool {
        self.owner.record() == owner && self.source == *source
    }
}

#[derive(Clone)]
enum NativeOwner {
    Server(chat::native_holder::Detached),
    Actor {
        record: registry::Record,
        handle: Handle,
    },
}
impl NativeOwner {
    fn record(&self) -> &registry::Record {
        match self {
            Self::Server(owner) => &owner.record,
            Self::Actor { record, .. } => record,
        }
    }
    fn close_retaining_receipt(&self) -> Result<(), String> {
        match self {
            Self::Server(owner) => owner.close_retaining_receipt(),
            Self::Actor { handle, .. } => handle.close_for_resume(),
        }
    }
}

#[derive(Clone)]
struct RetainedPair {
    source: Option<crate::model::TerminalSession>,
    holder: crate::terminal::holder::Record,
    handle: Option<crate::terminal::TerminalHandle>,
}

/// Cleanup does not require provider history or a successful protocol attach.
/// Only the authenticated exact holder and current exclusive daemon lease can
/// authorize it; disappearance alone never counts as a successful close.
pub(in crate::host) fn close_retained(host: &SharedHost, source: &Session) -> Result<(), String> {
    if retained_actor::reconcile_startup(host, source)? {
        return Ok(());
    }
    close_retained_inner(host, source, false, None)
}

/// A proved-dead provider can be restarted without closing its accepted agent
/// or dropping the old transcript. Live retained owners must be reattached.
pub(super) fn close_for_restart(host: &SharedHost, source: &Session) -> Result<(), String> {
    close_retained_inner(host, source, true, None)
}

pub(in crate::host) fn close_with_presentation(host: &SharedHost,
    origin: &crate::agent_execution::Origin, target: &crate::agent_execution::PresentationCloseTarget)
    -> Result<(), String> {
    let crate::agent_execution::HostedSession::Chat(source) = &target.observation.source else {
        return Err("Combined close requires an exact native provider".into());
    };
    close_retained_inner(host, source, false, Some((origin, target)))
}

fn close_retained_inner(host: &SharedHost, source: &Session, restart: bool,
    consent: Option<(&crate::agent_execution::Origin, &crate::agent_execution::PresentationCloseTarget)>) -> Result<(), String> {
    if let Some((origin, target)) = consent {
        let store = super::super::agent_execution::registry(host)?;
        store.lock().unwrap_or_else(|p| p.into_inner())
            .preflight_presentation_close(origin, &target.observation)?;
        super::presentation_close::require_current(host, target)?;
    } else if !restart {
        super::super::agent_execution::preflight_retirement(host, source)?;
    }
    let (proof, pending, actor) = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down
            || state.chat_spawning.contains_key(&source.id)
            || state.chat_handoffs.contains_key(&source.id)
        {
            return Err("Native ownership changed or another transition is pending; close the current conversation instead".into());
        }
        let actor = if let Some(record) = state.chats.get(&source.id) {
            if record.handle.owns_work() && consent.is_none() && !(restart && record.handle.is_completed()) {
                return Err("The native actor still owns work; use its existing close path".into());
            }
            let ServerResponse::ChatAttached {
                session: observed, ..
            } = session_snapshot(record)
            else {
                unreachable!()
            };
            if record.session != *source
                && observed != *source
                && !state
                    .upgrade_chats
                    .get(&source.id)
                    .is_some_and(|aliases| aliases.iter().any(|alias| alias.source == *source))
            {
                return Err(
                    "The completed native actor does not match this exact descriptor".into(),
                );
            }
            Some(record.handle.clone())
        } else {
            None
        };
        if restart && actor.as_ref().is_none_or(|handle| !handle.is_completed()) {
            return Err("Only the exact completed native actor can prepare a restart".into());
        }
        let pending = state.native_closing.get(&source.id).cloned();
        if pending
            .as_ref()
            .is_some_and(|pending| pending.source != *source)
        {
            return Err("Native cleanup is reserved by a different exact descriptor".into());
        }
        if pending.is_none() && state.native_closing.len() >= 128 {
            return Err(
                "Unconfirmed native cleanup owners reached their capacity; retry their close first"
                    .into(),
            );
        }
        state.chat_spawning.insert(source.id, source.clone());
        state.inventory_generation += 1;
        (state.proof().clone(), pending, actor)
    };
    let result = (|| {
        let socket = registry::socket(&registry::directory(Kind::Native)?, source.id);
        let receipt = chat::native_holder::closed_source(source, &proof)?;
        let owner = pending.as_ref().map(|pending| pending.owner.record().clone())
            .or_else(|| receipt.as_ref().map(|(record, _)| record.clone()))
            .map(Ok).unwrap_or_else(|| registry::evidence(&socket, Kind::Native)
                .and_then(|record| record.ok_or_else(|| "No exact native holder or confirmed close receipt is available".to_owned())))?;
        if !restart { preflight_current_retirements(host, source, &owner, &proof, consent)?; }
        if let Some((origin, target)) = consent {
            super::presentation_close::require_current(host, target)?;
            let store = super::super::agent_execution::registry(host)?;
            store.lock().unwrap_or_else(|p| p.into_inner())
                .preflight_presentation_close(origin, &target.observation)?;
        }
        if let Some((record, true)) = &receipt {
            if Handle::is_remote_record(record) && record.holder.is_current() {
                let handle = match pending.as_ref().map(|pending| &pending.owner) {
                    Some(NativeOwner::Actor { handle, .. }) => handle.clone(),
                    _ => Handle::adopt_remote(record, source.clone(), proof.clone(), |_| {})?,
                };
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .native_closing
                    .insert(
                        source.id,
                        NativeClose {
                            source: source.clone(),
                            owner: NativeOwner::Actor {
                                record: record.clone(),
                                handle: handle.clone(),
                            },
                            consented_backend: captured_backend(consent),
                            closed: true,
                            paired: None,
                        },
                    );
                retained_actor::finish_close(host, &handle, source)?;
            }
            if !restart {
                retire_closed_execution(host, source, record, &proof, consent)?;
            }
            return Ok(());
        }
        let mut pending = match pending {
            Some(pending) => pending,
            None if receipt.is_some() => {
                let (record, _) = receipt.unwrap();
                let pending = NativeClose {
                    source: source.clone(),
                    owner: NativeOwner::Server(chat::native_holder::Detached {
                        record,
                        source: source.clone(),
                        proof: proof.clone(),
                    }),
                    closed: true,
                    consented_backend: captured_backend(consent),
                    paired: retained_pair(
                        host,
                        source.terminal.as_ref(),
                        Some(&chat::native_holder::closed_pair_holder(source, &proof)?.ok_or("The native cleanup receipt lacks its original paired holder identity")?),
                        &proof,
                    )?,
                };
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .native_closing
                    .insert(source.id, pending.clone());
                pending
            }
            None if consent.is_some() && actor.is_some() => {
                let handle = actor.as_ref().unwrap().clone();
                let (paired_source, paired_holder) = if Handle::is_remote_record(&owner) {
                    let inspected = Handle::inspect_remote(&owner, proof.clone())?;
                    (inspected.source.terminal, inspected.paired_holder)
                } else {
                    let recovery = chat::native_holder::inspect(&owner, &proof)?.0;
                    (recovery.source.terminal, recovery.paired_holder)
                };
                let paired = retained_pair(host, paired_source.as_ref(), paired_holder.as_ref(), &proof)?;
                let pending = NativeClose {source: source.clone(),
                    owner: NativeOwner::Actor {record: owner.clone(), handle}, closed: false, paired, consented_backend: captured_backend(consent)};
                host.lock().unwrap_or_else(|p| p.into_inner()).native_closing.insert(source.id, pending.clone());
                pending
            }
            None => {
                let record = registry::evidence(&socket, Kind::Native)?
                    .ok_or("No exact native holder or confirmed close receipt is available")?;
                if Handle::is_remote_record(&record) {
                    if restart {
                        return Err(
                            "The retained Claude actor must restart inside its existing owner"
                                .into(),
                        );
                    }
                    let inspection = Handle::inspect_remote(&record, proof.clone())?;
                    let paired = retained_pair(
                        host,
                        inspection.source.terminal.as_ref(),
                        inspection.paired_holder.as_ref(),
                        &proof,
                    )?;
                    // The helper independently recognizes the entire issued
                    // descriptor before changing attachment epoch or closing.
                    let handle =
                        Handle::adopt_remote(&record, source.clone(), proof.clone(), |_| {})?;
                    let pending = NativeClose {
                        source: source.clone(),
                        owner: NativeOwner::Actor { record, handle },
                        consented_backend: captured_backend(consent),
                        closed: false,
                        paired,
                    };
                    host.lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .native_closing
                        .insert(source.id, pending.clone());
                    pending
                } else {
                    let (recovery, exited) = chat::native_holder::inspect(&record, &proof)?;
                    if restart && !exited {
                        return Err("The retained provider is still alive; reattach its existing session instead of starting another writer".into());
                    }

                    if &recovery.source != source && !recovery.previous_sources.contains(source) {
                        return Err(
                            "Native holder does not recognize this exact issued descriptor".into(),
                        );
                    }
                    let paired = retained_pair(
                        host,
                        recovery.source.terminal.as_ref(),
                        recovery.paired_holder.as_ref(),
                        &proof,
                    )?;
                    let (holder, _) = chat::native_holder::adopt(&record, source, &proof)?;
                    let pending = NativeClose {
                        source: source.clone(),
                        owner: NativeOwner::Server(holder.preserve()),
                        consented_backend: captured_backend(consent),
                        closed: false,
                        paired,
                    };
                    host.lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .native_closing
                        .insert(source.id, pending.clone());
                    pending
                }
            }
        };
        if let Some(pair) = &pending.paired {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            if state
                .terminals
                .get(&pair.holder.terminal)
                .is_some_and(|current| {
                    pair.handle
                        .as_ref()
                        .is_none_or(|expected| !current.handle.same_worker(expected))
                })
            {
                return Err(
                    "The paired terminal was replaced; its replacement was not stopped".into(),
                );
            }
        }
        if !pending.closed {
            pending.owner.close_retaining_receipt()?;
            pending.closed = true;
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .native_closing
                .insert(source.id, pending.clone());
        }
        if let Some(pair) = &pending.paired {
            if let Some(handle) = &pair.handle {
                handle.close_with_receipt(
                    pair.source.as_ref().expect("captured worker source"),
                    &proof,
                )?;
            } else if !crate::terminal::holder::cleanup::confirmed_owner(&pair.holder, &proof)? {
                return Err(
                    "Paired terminal teardown is not proved; its ownership was retained".into(),
                );
            }
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if state
                .terminals
                .get(&pair.holder.terminal)
                .is_some_and(|current| {
                    pair.handle
                        .as_ref()
                        .is_none_or(|handle| !current.handle.same_worker(handle))
                })
            {
                return Err(
                    "The paired terminal changed during cleanup; its replacement was retained"
                        .into(),
                );
            }
            state.retire_terminal(pair.holder.terminal);
        }
        chat::native_holder::confirm_pair_closed(source, &proof)?;
        if let NativeOwner::Actor { handle, .. } = &pending.owner {
            retained_actor::finish_close(host, handle, source)?;
        }
        // Keep the proved owner while durable retirement is retryable. An SQL
        // failure cannot force a second Release or lose its cleanup evidence.
        if !restart {
            retire_closed_execution(host, source, pending.owner.record(), &proof, consent)?;
        }
        Ok(())
    })();
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    state.chat_spawning.remove(&source.id);
    state.inventory_generation += 1;
    if result.is_ok() {
        if let Some(actor) = &actor {
            if state
                .chats
                .get(&source.id)
                .is_some_and(|record| !record.handle.same_actor(actor))
            {
                return Err(
                    "The native actor was replaced during cleanup; its replacement was retained"
                        .into(),
                );
            }
            state.issued_chat_ids.insert(source.id);
        }
        let retired_actor = if !restart && actor.is_some() { state.chats.remove(&source.id) } else { None };
        let retired_close = state.native_closing.remove(&source.id);
        state.native_adoption_failures.clear(source.id);
        if !restart {
            if let Some(notifications) = &state.notifications {
                notifications.retire(source.id);
            }
        }
        drop(state);
        // Local Store/cache and paired terminal owners can be expensive last
        // drops. Never destroy them while the global host census is locked.
        #[cfg(test)]
        if let Some(hook) = PRESENTATION_CLOSE_BEFORE_OWNER_DROP.lock().unwrap().take() { hook(); }
        drop(retired_actor);
        drop(retired_close);
        drop(actor);
        if !restart {
            super::super::state::retire_unattached_upgrade_chat(host, source.id);
        }
    }
    result
}

/// The caller may be an authenticated old alias, while the agent already
/// retains the adopted descriptor. Authenticate and preflight the same CURRENT
/// source set that retirement will use, before adoption or Release changes it.
fn preflight_current_retirements(
    host: &SharedHost,
    caller: &Session,
    owner: &registry::Record,
    proof: &HostSessionProof,
    consent: Option<(&crate::agent_execution::Origin, &crate::agent_execution::PresentationCloseTarget)>,
) -> Result<(), String> {
    let Ok(store) = super::super::agent_execution::registry(host) else { return Ok(()); };
    let sources = store.lock().unwrap_or_else(|p| p.into_inner()).retained_agent_sources(caller.id)?;
    if sources.is_empty() { return Ok(()); }
    let mut lineage = None;
    for retained in sources {
        let crate::agent_execution::HostedSession::Chat(current) = &retained else {
            return Err("Retained execution ownership changed to another surface".into());
        };
        let recognized_receipt = chat::native_holder::closed_source(current, proof)?
            .is_some_and(|(record, _)| record == *owner);
        if !recognized_receipt {
            if lineage.is_none() {
                let (source, previous) = if Handle::is_remote_record(owner) {
                    let recovery = Handle::inspect_remote(owner, proof.clone())?;
                    (recovery.source, recovery.previous_sources)
                } else {
                    let recovery = chat::native_holder::inspect(owner, proof)?.0;
                    (recovery.source, recovery.previous_sources)
                };
                if source != *caller && !previous.contains(caller) {
                    return Err("Native holder does not recognize this exact issued descriptor".into());
                }
                lineage = Some((source, previous));
            }
            let (source, previous) = lineage.as_ref().unwrap();
            if source != current && !previous.contains(current) {
                return Err("The current execution descriptor is not covered by this exact native holder lineage; no teardown was requested".into());
            }
        }
        let mut store = store.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((origin, target)) = consent
            && retained == target.observation.source {
            store.preflight_presentation_close(origin, &target.observation)?;
        } else {
            store.preflight_agent_retirement(&retained)?;
        }
    }
    Ok(())
}

fn retire_closed_execution(
    host: &SharedHost,
    source: &Session,
    owner: &registry::Record,
    proof: &HostSessionProof,
    consent: Option<(&crate::agent_execution::Origin, &crate::agent_execution::PresentationCloseTarget)>,
) -> Result<(), String> {
    let Ok(registry) = super::super::agent_execution::registry(host) else {
        return Ok(());
    };
    let sources = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retained_agent_sources(source.id)?;
    let mut verified = Vec::new();
    for source in sources {
        let crate::agent_execution::HostedSession::Chat(source) = source else {
            return Err("Retained execution ownership changed to another surface".into());
        };
        if !chat::native_holder::closed_source(&source, proof)?
            .is_some_and(|(record, paired_closed)| record == *owner && paired_closed)
        {
            return Err("The current execution descriptor is not covered by this exact native teardown receipt".into());
        }
        verified.push(source);
    }
    if let Some((origin, target)) = consent {
        super::presentation_close::require_current(host, target)?;
        #[cfg(test)]
        if let Some(hook) = PRESENTATION_CLOSE_BEFORE_COMMIT.lock().unwrap().take() { hook(); }
        let expected = &target.observation.source;
        if !verified.iter().any(|source| crate::agent_execution::HostedSession::Chat(source.clone()) == *expected) {
            return Err("The confirmed native receipt does not cover this exact consented agent".into());
        }
        let verified_sources = verified.iter().cloned().map(crate::agent_execution::HostedSession::Chat).collect::<Vec<_>>();
        registry.lock().unwrap_or_else(|p| p.into_inner())
            .retire_agent_process_with_presentation(origin, expected, chat::now_ms().unwrap_or(0), &target.observation, &verified_sources)?;
        #[cfg(test)]
        if let Some(hook) = PRESENTATION_CLOSE_AFTER_COMMIT.lock().unwrap().take() { hook(); }
        super::super::agent_execution::announce(host, &registry);
        // Agent, current authorities and pane already committed together.
        // Never make a redundant writer transaction turn success into a close
        // error that would retain an owner behind an already-closed agent.
        return Ok(());
    }
    for source in verified {
        super::super::agent_execution::retired(host, &source)?;
    }
    Ok(())
}

fn retained_pair(
    host: &SharedHost,
    terminal: Option<&crate::model::TerminalSession>,
    holder: Option<&crate::terminal::holder::Record>,
    proof: &HostSessionProof,
) -> Result<Option<RetainedPair>, String> {
    if terminal.is_none() && holder.is_none() {
        return Ok(None);
    }
    let holder = holder.ok_or("This older paired native holder has no exact terminal-owner evidence; automatic paired cleanup was refused")?;
    if terminal.is_some_and(|terminal| terminal.id != holder.terminal) {
        return Err("The paired source differs from its authenticated holder identity".into());
    }
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(current) = state.terminals.get(&holder.terminal) {
        if current.handle.holder_identity() != Some(holder) {
            return Err(
                "The original paired terminal holder was replaced; its replacement was not stopped"
                    .into(),
            );
        }
        let descriptor = state
            .descriptor(0, holder.terminal)
            .map_err(|error| error.message)?;
        if terminal.is_some_and(|terminal| {
            descriptor.session != *terminal && !descriptor.upgrade_sources.contains(terminal)
        }) {
            return Err(
                "The paired terminal does not retain this exact native holder's source identity"
                    .into(),
            );
        }
        return Ok(Some(RetainedPair {
            source: Some(descriptor.session),
            holder: holder.clone(),
            handle: Some(current.handle.clone()),
        }));
    }
    drop(state);
    // The native holder's authenticated recovery/receipt owns this exact
    // terminal incarnation, even when the caller predates pair publication.
    if !crate::terminal::holder::cleanup::confirmed_owner(holder, proof)? {
        return Err(
            "The original paired terminal is unavailable and its teardown is not proved".into(),
        );
    }
    Ok(Some(RetainedPair {
        source: None,
        holder: holder.clone(),
        handle: None,
    }))
}

// Deterministic test scheduling seam after genuine cleanup proof, before SQL
// CAS. It cannot manufacture a receipt or bypass any production predicate.
#[cfg(test)]
static PRESENTATION_CLOSE_BEFORE_COMMIT: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> = std::sync::Mutex::new(None);
#[cfg(test)]
pub(in crate::host) fn before_presentation_close_commit(hook: impl FnOnce() + Send + 'static) {
    *PRESENTATION_CLOSE_BEFORE_COMMIT.lock().unwrap() = Some(Box::new(hook));
}

fn captured_backend(consent: Option<(&crate::agent_execution::Origin, &crate::agent_execution::PresentationCloseTarget)>)
    -> Option<registry::Process> {
    match consent?.1.backend {
        crate::agent_execution::PresentationCloseBackend::Process {pid, start} => Some(registry::Process {pid, start}),
        crate::agent_execution::PresentationCloseBackend::RetiredOwner => None,
    }
}

#[cfg(test)]
static PRESENTATION_CLOSE_BEFORE_OWNER_DROP: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> = std::sync::Mutex::new(None);
#[cfg(test)]
pub(in crate::host) fn before_presentation_close_owner_drop(hook: impl FnOnce() + Send + 'static) {
    *PRESENTATION_CLOSE_BEFORE_OWNER_DROP.lock().unwrap() = Some(Box::new(hook));
}

#[cfg(test)]
static PRESENTATION_CLOSE_AFTER_COMMIT: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> = std::sync::Mutex::new(None);
#[cfg(test)]
pub(in crate::host) fn after_presentation_close_commit(hook: impl FnOnce() + Send + 'static) {
    *PRESENTATION_CLOSE_AFTER_COMMIT.lock().unwrap() = Some(Box::new(hook));
}
