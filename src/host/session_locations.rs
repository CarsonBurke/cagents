//! Prepare process evidence outside HostState, then atomically relocate logical
//! ownership and its exact sessions. Caller holds exclusive control admission.
use super::{model::SessionMove, state::{ClientId, SharedHost}};
use crate::{agent_execution::HostedSession, chat::runtime::{DirectoryProof, Handle}, model::{CardKind, TerminalId}, project_recovery::Relocation, terminal::TerminalHandle, terminal_ipc::{ProtocolError, ProtocolErrorCode}, workspace_model::{ModelEdit, WorkspaceModel, views::Scope}};
use std::{collections::BTreeMap, path::Path, time::{Duration, Instant}};

#[derive(Clone)]
enum Owner { Chat(Handle), Terminal(TerminalHandle), Unavailable }
struct Prepared { movement: SessionMove, owner: Owner, proof: Option<DirectoryProof> }

fn error(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}
fn issued_directory(source: &HostedSession) -> &Path {
    match source { HostedSession::Chat(source) => &source.launch.recipe.working_directory, HostedSession::Terminal(source) => &source.launch.working_directory }
}
fn owned(scope: &Scope, model: &WorkspaceModel, relocation: &Relocation) -> bool {
    match scope {
        Scope::Worktree { repository, .. } => *repository == relocation.repository,
        Scope::Channel(id) => model.channel(*id).is_some_and(|channel| channel.repository == relocation.repository),
    }
}

pub(super) fn relocate(host: &SharedHost, origin: ClientId, relocation: Relocation) -> Result<WorkspaceModel, ProtocolError> {
    let edit = ModelEdit::RelocateProject { relocation }.canonicalized().map_err(error)?;
    let ModelEdit::RelocateProject { relocation } = &edit else { unreachable!() };
    let registry = super::agent_execution::registry(host).map_err(error)?;
    let sources = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let model = state.read_model()?;
        let mut sources = BTreeMap::<TerminalId, HostedSession>::new();
        for view in &model.views {
            if !owned(&view.scope, &model, relocation) { continue; }
            for pane in &view.panes {
                match &pane.kind {
                    CardKind::Chat(session) if state.chats.contains_key(&session.id) || !session.launch.needs_read_only_history() => { sources.insert(session.id, HostedSession::Chat(session.clone())); },
                    CardKind::Terminal(session) if session.continuity.live_proof().is_some() => { sources.entry(session.id).or_insert_with(|| HostedSession::Terminal(session.clone())); },
                    _ => {},
                }
            }
        }
        for agent in &model.agents {
            let repository = model.agent_locations.iter().find(|location| location.agent == agent.id)
                .map(|location| &location.repository).unwrap_or(&agent.spec.target.repository);
            if *repository == relocation.repository && let Some(source) = &agent.session {
                sources.insert(source.id(), source.clone());
            }
        }
        // Exact host records supersede cached presentation descriptors. No
        // source recipe, holder checkpoint or observed shell cwd is rewritten.
        sources.into_iter().map(|(id, source)| {
            if let Some(record) = state.chats.get(&id) {
                (HostedSession::Chat(record.session.clone()), Owner::Chat(record.handle.clone()))
            } else if let Some(record) = state.terminals.get(&id) {
                (HostedSession::Terminal(record.session.clone()), Owner::Terminal(record.handle.clone()))
            } else { (source, Owner::Unavailable) }
        }).collect::<Vec<_>>()
    };
    let mut prepared = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    for (source, owner) in sources {
        let previous = registry.lock().unwrap_or_else(|p| p.into_inner()).session_location(&source).map_err(error)?;
        let directory = previous.as_ref().map(|location| location.blocked_directory.as_deref().unwrap_or(&location.directory)).unwrap_or_else(|| issued_directory(&source));
        let candidate = relocation.mapped(directory);
        let approved = previous.as_ref().and_then(|location| location.approved);
        let proof = if Instant::now() >= deadline {
            Err("Locate reached its provider verification time limit; retry after these providers are responsive, or close the affected panes explicitly".into())
        } else { match &owner {
            Owner::Chat(handle) => handle.capture_execution_directory(&candidate),
            Owner::Terminal(handle) => handle.holder_identity().ok_or_else(|| "The original terminal has no authenticated holder identity; close this pane explicitly or retry after recovery".to_owned()).and_then(|holder| {
                if !holder.holder.is_current() { return Err("The original terminal holder is no longer live; recover or close this pane before retrying Locate".into()); }
                DirectoryProof::capture_owned_process(&holder.root, &candidate, approved)
            }),
            Owner::Unavailable => Err("The original provider is not attached to this daemon. Recover its exact holder, or close this pane and start a new session in the located folder".into()),
        }}.and_then(|proof| {
            if approved.is_some_and(|identity| proof.identity != identity) {
                Err("The provider directory differs from its original approved directory; close it before starting in a replacement folder".into())
            } else { Ok(proof) }
        });
        let movement = SessionMove { source, repository: relocation.destination.clone(), candidate, proof: proof.as_ref().map(|proof| (proof.identity, proof.process.clone())).map_err(Clone::clone) };
        prepared.push(Prepared { movement, owner, proof: proof.ok() });
    }
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        for item in &prepared {
            let id = item.movement.source.id();
            let unchanged = match (&item.owner, &item.movement.source) {
                (Owner::Chat(handle), HostedSession::Chat(source)) => state.chats.get(&id).is_some_and(|record| record.handle.same_actor(handle) && HostedSession::Chat(record.session.clone()).same_process(&HostedSession::Chat(source.clone()))),
                (Owner::Terminal(handle), HostedSession::Terminal(source)) => state.terminals.get(&id).is_some_and(|record| record.handle.same_worker(handle) && HostedSession::Terminal(record.session.clone()).same_process(&HostedSession::Terminal(source.clone()))),
                (Owner::Unavailable, _) => !state.chats.contains_key(&id) && !state.terminals.contains_key(&id),
                _ => false,
            };
            if !unchanged { return Err(error("A hosted session changed while Locate verified its directory; retry without closing or replacing it")); }
        }
        let moves = prepared.iter().map(|item| item.movement.clone()).collect::<Vec<_>>();
        state.apply_session_relocation(&edit, &moves)?;
    }
    // The exclusive gate excludes further edits/starts while actor-local
    // operational paths catch up. Keep any failed actor move explicitly blocked.
    for item in &mut prepared {
        if item.proof.is_some() && let Owner::Chat(handle) = &item.owner {
            if let Err(reason) = handle.relocate_execution_directory(&item.movement.candidate) {
                registry.lock().unwrap_or_else(|p| p.into_inner()).refuse_session_location(&item.movement.source, &item.movement.candidate, &reason).map_err(error)?;
            }
        }
    }
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let model = state.read_model()?;
    state.announce_model_edit(origin, model.sequence, &edit);
    Ok(model)
}
