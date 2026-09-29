//! Exact, retryable cleanup when a retained provider cannot attach its history.
use super::*;
use crate::daemon::holder_registry::{self as registry, Kind};

#[derive(Clone)]
pub(in crate::host) struct NativeClose {
    source: Session,
    owner: chat::native_holder::Detached,
    closed: bool,
    paired: Option<RetainedPair>,
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
    let (proof, pending, actor) = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down
            || state.chat_spawning.contains_key(&source.id)
            || state.chat_handoffs.contains_key(&source.id)
        {
            return Err("Native ownership changed or another transition is pending; close the current conversation instead".into());
        }
        let actor = if let Some(record) = state.chats.get(&source.id) {
            if record.handle.owns_work() {
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
        if let Some((record, true)) = &receipt {
            retire_closed_execution(host, source, record, &proof)?;
            return Ok(());
        }
        let mut pending = match pending {
            Some(pending) => pending,
            None if receipt.is_some() => {
                let (record, _) = receipt.unwrap();
                let pending = NativeClose {
                    source: source.clone(),
                    owner: chat::native_holder::Detached {
                        record,
                        source: source.clone(),
                        proof: proof.clone(),
                    },
                    closed: true,
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
            None => {
                let record = registry::evidence(&socket, Kind::Native)?
                    .ok_or("No exact native holder or confirmed close receipt is available")?;
                let (recovery, _) = chat::native_holder::inspect(&record, &proof)?;
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
                    owner: holder.preserve(),
                    closed: false,
                    paired,
                };
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .native_closing
                    .insert(source.id, pending.clone());
                pending
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
        // Keep the proved owner while durable retirement is retryable. An SQL
        // failure cannot force a second Release or lose its cleanup evidence.
        retire_closed_execution(host, source, &pending.owner.record, &proof)?;
        Ok(())
    })();
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    state.chat_spawning.remove(&source.id);
    state.inventory_generation += 1;
    if result.is_ok() {
        if let Some(actor) = actor {
            if state
                .chats
                .get(&source.id)
                .is_some_and(|record| !record.handle.same_actor(&actor))
            {
                return Err(
                    "The native actor was replaced during cleanup; its replacement was retained"
                        .into(),
                );
            }
            state.chats.remove(&source.id);
        }
        state.native_closing.remove(&source.id);
        state.native_adoption_failures.clear(source.id);
        if let Some(notifications) = &state.notifications {
            notifications.retire(source.id);
        }
        drop(state);
        super::super::state::retire_unattached_upgrade_chat(host, source.id);
    }
    result
}

fn retire_closed_execution(
    host: &SharedHost,
    source: &Session,
    owner: &registry::Record,
    proof: &HostSessionProof,
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
