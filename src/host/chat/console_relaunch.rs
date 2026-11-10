//! Restore the console view, never the provider or an accepted prompt.
use super::*;
use crate::{
    agent_execution::{HostedSession, Surface},
    host::model::console_relaunch::ConsoleRelaunch,
    model::{LaunchRecipe, TerminalContinuity, TerminalSession},
    terminal::holder,
};

pub(super) struct Prepared {
    pub intent: ConsoleRelaunch,
    pub terminal: TerminalSession,
    pub handle: crate::terminal::TerminalHandle,
    pub sources: Vec<Session>,
}

#[cfg(test)]
thread_local! {
    static FAIL_AFTER: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn fail_after_for_test(point: &'static str) {
    FAIL_AFTER.with(|slot| slot.set(Some(point)));
}

pub(super) fn checkpoint(_point: &'static str) -> Result<(), String> {
    #[cfg(test)]
    if FAIL_AFTER.with(|slot| {
        slot.get() == Some(_point) && {
            slot.set(None);
            true
        }
    }) {
        return Err(format!("fixture crash after console {_point}"));
    }
    Ok(())
}

fn matched_cli() -> Result<std::path::PathBuf, String> {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("CAGENTS_TEST_CONSOLE_CLI") {
        let path = std::path::PathBuf::from(path);
        if path.is_absolute() && path.is_file() {
            return Ok(path);
        }
        return Err("The fixture's matched console executable is unavailable".into());
    }
    let program = crate::companion::Product::Cli
        .beside_current()
        .map_err(|e| e.to_string())?;
    if !program.is_file() {
        return Err("The matched cagents CLI for console recovery is unavailable".into());
    }
    Ok(program)
}

fn detached(proof: &HostSessionProof) -> TerminalContinuity {
    TerminalContinuity::DetachedSameBoot {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    }
}

/// Only the helper's exact execution-console pairing and a durable agent
/// binding authorize this operation. Merely recognizing console argv does not.
pub(super) fn prepare(
    host: &SharedHost,
    handle: &Handle,
    requested: &Session,
    proof: &HostSessionProof,
    source: &Session,
    previous: &[Session],
    console: Option<&LaunchRecipe>,
    pair: Option<&holder::Record>,
) -> Result<Option<Prepared>, String> {
    let identity = handle
        .actor_identity()
        .ok_or("Console recovery lost its retained actor")?;
    let registry = super::super::agent_execution::registry(host)?;
    let pending = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .console_relaunch(identity.instance)?;
    if pending.is_none()
        && console
            .and_then(crate::agent_execution::console_agent)
            .is_none()
    {
        return Ok(None);
    }
    let result = (|| {
        let mut sources = previous.to_vec();
        sources.extend([source.clone(), requested.clone()]);
        let mut intent = if let Some(intent) = pending {
            if intent.owner != identity.record || intent.source.id != source.id {
                return Err("Console replacement belongs to another exact retained actor".into());
            }
            handle.recognize_retained_sources(&[intent.source.clone()])?;
            sources.push(intent.source.clone());
            intent
        } else {
            let terminal = source
                .terminal
                .as_ref()
                .ok_or("The retained console has no issued view")?;
            let pair = pair.ok_or("The retained console has no exact holder")?;
            if console != Some(&terminal.launch)
                || terminal.id != source.id
                || terminal.owned_codex_listener.is_some()
            {
                return Err("The retained execution console changed its issued pairing".into());
            }
            let agent_id = crate::agent_execution::console_agent(&terminal.launch)
                .ok_or("The retained view is not an agent console")?;
            let (agent, ownership) = {
                let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
                (
                    registry.agent(agent_id)?,
                    registry
                        .native_actor_ownership(identity.instance)?
                        .ok_or("The retained console provider has no durable owner")?,
                )
            };
            if agent.closed
                || agent.spec.surface != Surface::Terminal
                || agent.spec.provider != Provider::Claude
                || agent
                    .console
                    .as_ref()
                    .is_none_or(|old| old.id != terminal.id || old.launch != terminal.launch)
                || ownership.owner() != &identity.record
            {
                return Err("The retained view does not own this agent's console".into());
            }
            let HostedSession::Chat(owned) = agent
                .session
                .clone()
                .ok_or("The console agent has no provider binding")?
            else {
                return Err("The console agent is not bound to its retained provider".into());
            };
            sources.extend([owned, ownership.source().clone()]);
            handle.recognize_retained_sources(&sources)?;
            let (worker, options, aliases) = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                let worker = state
                    .terminals
                    .get(&terminal.id)
                    .map(|record| {
                        if record.handle.holder_identity() != Some(pair)
                            || record.session.launch != terminal.launch
                        {
                            return Err(
                                "Another terminal owns the retained console's ID".to_owned()
                            );
                        }
                        Ok((record.handle.clone(), record.session.clone()))
                    })
                    .transpose()?;
                if source.proof.as_ref() == Some(proof)
                    && worker
                        .as_ref()
                        .is_some_and(|(handle, _)| !handle.is_completed())
                {
                    return Ok(None);
                }
                let aliases = state
                    .descriptor(0, terminal.id)
                    .ok()
                    .map(|d| d.upgrade_sources)
                    .unwrap_or_default();
                (
                    worker,
                    state.terminal_options(terminal.id).unwrap_or_default(),
                    aliases,
                )
            };
            // Closing this thin CLI never closes its separately owned provider.
            // The durable receipt is obtained before the replacement intent.
            if let Some((worker, issued)) = worker {
                worker.close_with_receipt(&issued, proof)?;
            } else {
                let original = source
                    .proof
                    .as_ref()
                    .ok_or("The console lacks its issuing daemon proof")?;
                let role = crate::daemon::holder_registry::OriginalRole::authenticated(
                    pair, original, proof,
                )?;
                if !holder::cleanup::confirmed_owner_with_role(pair, proof, role.as_ref())? {
                    return Err("The original console has not proved its exact release".into());
                }
            }
            let original = agent.console.unwrap();
            let mut target = original.clone();
            target.launch.program = matched_cli()?
                .into_os_string()
                .into_string()
                .map_err(|_| "Console executable path is not UTF-8")?;
            target.continuity = detached(proof);
            target.resume_hint = None;
            let mut aliases: Vec<_> = aliases
                .into_iter()
                .chain(sources.iter().filter_map(|s| s.terminal.clone()))
                .chain(std::iter::once(original.clone()))
                .filter(|alias| alias.continuity.live_proof().is_some())
                .collect();
            aliases.sort_by_key(|alias| serde_json::to_vec(alias).unwrap_or_default());
            aliases.dedup();
            let intent = ConsoleRelaunch {
                actor: identity.instance,
                owner: identity.record.clone(),
                agent: agent_id,
                source: source.clone(),
                console: original,
                closed_holder: pair.clone(),
                target,
                options: crate::terminal_ipc::TerminalSpawnOptions {
                    initially_visible: false,
                    ..options
                },
                aliases,
                replacement_holder: None,
                closed_replacement: None,
            };
            registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .begin_console_relaunch(
                    &intent,
                    &sources
                        .iter()
                        .cloned()
                        .map(HostedSession::Chat)
                        .collect::<Vec<_>>(),
                )?;
            intent
        };
        // A real replacement CLI stops waiting when its launching daemon
        // loses the lease. Prove that exact view closed before retrying under
        // this daemon; the independently retained provider is never stopped.
        let ended = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.terminals.get(&intent.source.id).and_then(|record| {
                ((record.handle.is_completed()
                    || intent.target.continuity.live_proof().as_ref() != Some(proof))
                    && intent.recognizes_issued_target(&record.session)
                    && record.handle.holder_identity().is_some_and(|owner| {
                        intent
                            .replacement_holder
                            .as_ref()
                            .is_none_or(|expected| expected == owner)
                    }))
                .then(|| {
                    (
                        record.handle.clone(),
                        record.session.clone(),
                        record.handle.holder_identity().unwrap().clone(),
                    )
                })
            })
        };
        if let Some((worker, issued, owner)) = ended {
            worker.close_with_receipt(&issued, proof)?;
            intent = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retry_console_relaunch(&intent, &owner, &issued, proof)?;
        } else if let Some(owner) = &intent.replacement_holder {
            let present = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .terminals
                .contains_key(&intent.source.id);
            if !present {
                let original = intent
                    .target
                    .continuity
                    .live_proof()
                    .ok_or("The issued replacement lacks its startup owner")?;
                let role = crate::daemon::holder_registry::OriginalRole::authenticated(
                    owner, &original, proof,
                )?;
                if holder::cleanup::confirmed_owner_with_role(owner, proof, role.as_ref())? {
                    intent = registry
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .retry_console_relaunch(&intent, owner, &intent.target, proof)?;
                }
            }
        }
        // Old protocol-2 actors require a distinct unpair ACK before a new
        // terminal holder may be installed. Both sides retain the old aliases.
        if pair == Some(&intent.closed_holder)
            || pair.is_some() && pair == intent.closed_replacement.as_ref()
        {
            let mut unpaired = source.clone();
            unpaired.proof = Some(proof.clone());
            unpaired.terminal = None;
            handle.update_native_source(unpaired.clone(), None, None)?;
            checkpoint("unpair")?;
            sources.push(unpaired);
        } else if pair.is_none() {
            if console.is_some() || source.terminal.is_some() {
                return Err("The unpaired console helper returned inconsistent ownership".into());
            }
        } else if intent.replacement_holder.as_ref() != pair {
            return Err("The retained actor names an unknown replacement console holder".into());
        }
        let existing = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(record) = state.terminals.get(&intent.source.id) {
                if record.handle.holder_identity() == Some(&intent.closed_holder)
                    || record.handle.holder_identity().is_some()
                        && record.handle.holder_identity() == intent.closed_replacement.as_ref()
                {
                    if !record.handle.is_completed() {
                        return Err("The closed console worker is still active".into());
                    }
                    state.retire_terminal(intent.source.id);
                    None
                } else {
                    let holder = record
                        .handle
                        .holder_identity()
                        .ok_or("The replacement console has no authenticated holder")?;
                    if !intent.recognizes_issued_target(&record.session)
                        || record.handle.is_completed()
                        || intent
                            .replacement_holder
                            .as_ref()
                            .is_some_and(|expected| expected != holder)
                    {
                        return Err(
                            "Another worker owns the console replacement's preissued target".into(),
                        );
                    }
                    Some(record.handle.clone())
                }
            } else {
                None
            }
        };
        let worker = if let Some(worker) = existing {
            worker
        } else {
            let socket = holder::socket(&holder::directory()?, intent.source.id);
            if crate::daemon::holder_registry::recovery_candidate(
                &socket,
                crate::daemon::holder_registry::Kind::Terminal,
            )?
            .is_some()
            {
                return Err("An unadopted console holder still owns the preissued target; no second view was started".into());
            }
            if intent.replacement_holder.is_some() {
                return Err("The issued replacement console is unavailable; its provider and intent were preserved".into());
            }
            let mut current = intent.clone();
            current.target.continuity = detached(proof);
            if current != intent {
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .update_console_relaunch(&intent, &current)?;
                intent = current;
            }
            super::super::state::spawn_console_replacement(
                host,
                intent.target.clone(),
                intent.options.clone(),
                &intent.aliases,
                requested,
            )
            .map_err(|e| e.message)?
            .1
        };
        checkpoint("spawn")?;
        let owner = worker
            .holder_identity()
            .ok_or("The replacement console did not install its holder")?
            .clone();
        if intent.replacement_holder.is_none() {
            let mut next = intent.clone();
            next.replacement_holder = Some(owner);
            registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .update_console_relaunch(&intent, &next)?;
            intent = next;
        }
        let terminal = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_terminal_projection(intent.source.id)?;
        if !intent.recognizes_target(&terminal) {
            return Err("The replacement console changed before native re-pairing".into());
        }
        Ok(Some(Prepared {
            intent,
            terminal,
            handle: worker,
            sources,
        }))
    })();
    if let Err(error) = &result {
        // A console failure cannot turn retained provider work into an
        // unproven run. Stop startup before its blanket interruption sweep.
        host.lock().unwrap_or_else(|p| p.into_inner()).retain_startup_recovery(format!("The console view could not reconnect; its provider and accepted work remain retained: {error}"));
    }
    result
}
