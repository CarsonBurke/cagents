use super::{Home, SharedHost, pending_operation, refusal};
use crate::{
    agent::Provider,
    model::{CardKind, LaunchRecipe},
    terminal_ipc::ProtocolError,
    workspace_model::{ModelEdit, views::Change},
};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

/// The reservation is already installed, so related starts and new shared
/// pane bindings cannot overtake this read.
pub(super) fn ensure_unoccupied(
    host: &SharedHost,
    provider: Provider,
    source: &Home,
    target: &[Home],
) -> Result<(), ProtocolError> {
    let (proof, starting, resident_chats, resident_terminals) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (
            state.proof().clone(),
            state.chat_spawning.values().cloned().collect::<Vec<_>>(),
            state.chats.keys().copied().collect::<BTreeSet<_>>(),
            state.terminals.keys().copied().collect::<BTreeSet<_>>(),
        )
    };
    for session in starting {
        let kind = CardKind::Chat(session);
        for home in std::iter::once(source).chain(target) {
            if uses_home(&kind, home)? {
                return Err(pending_operation(
                    "A conversation is still starting on this account. Close it before removing the account.",
                ));
            }
        }
    }

    let registry = super::super::agent_execution::registry(host).map_err(refusal)?;
    let panes = {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let mut after = None;
        loop {
            let owners = registry
                .native_actor_ownerships(after, 128)
                .map_err(refusal)?;
            if owners.is_empty() {
                break;
            }
            for owner in &owners {
                if owner.owner().boot != proof.boot_id
                    || owner.source().launch.provider != provider
                    || registry
                        .native_actor_retirement(owner.actor())
                        .map_err(refusal)?
                        .is_some()
                {
                    continue;
                }
                let scope = owner.scope().map_err(|error| {
                    refusal(format!(
                        "Cannot verify a retained conversation's account home: {error}"
                    ))
                })?;
                let conflict = std::iter::once(source).chain(target).any(|home| {
                    home.identity == Some((scope.device, scope.inode))
                        || scope.canonical_home.starts_with(&home.canonical)
                        || home.canonical.starts_with(&scope.canonical_home)
                });
                if conflict {
                    return Err(pending_operation(
                        "A retained conversation still uses this account. Reconnect and close it before removing the account.",
                    ));
                }
            }
            after = owners.last().map(|owner| owner.actor());
        }
        registry.open_account_panes().map_err(refusal)?
    };
    for pane in panes {
        require_closed(&pane, source)?;
    }
    ensure_no_detached_holders(
        host,
        &proof,
        source,
        target,
        &resident_chats,
        &resident_terminals,
    )?;

    let mut budget = crate::persistence::MigrationInspectionBudget::new(
        Instant::now() + Duration::from_secs(30),
    );
    for path in crate::instance::running_local_window_database_paths()
        .map_err(|error| refusal(error.to_string()))?
    {
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(refusal(error.to_string())),
        }
        let panes =
            crate::persistence::read_labeled_migration_sessions_with_budget(&path, &mut budget)
                .map_err(|error| refusal(error.to_string()))?;
        for pane in panes.into_iter().filter(|pane| !pane.inert) {
            require_closed(&pane.kind, source)?;
        }
    }
    Ok(())
}

fn ensure_no_detached_holders(
    host: &SharedHost,
    proof: &crate::model::HostSessionProof,
    source: &Home,
    target: &[Home],
    resident_chats: &BTreeSet<crate::model::TerminalId>,
    resident_terminals: &BTreeSet<crate::model::TerminalId>,
) -> Result<(), ProtocolError> {
    use crate::daemon::holder_registry::{self, Kind};
    let inspect_failed = |error| {
        pending_operation(format!(
            "Cannot verify a detached session's account. Reconnect or close it before removing the account: {error}"
        ))
    };
    for record in holder_registry::observe_account_owners(Kind::Native, &proof.boot_id)
        .map_err(|error| inspect_failed(error.to_string()))?
    {
        if resident_chats.contains(&record.terminal) {
            continue;
        }
        let session = if crate::chat::runtime::Handle::is_remote_record(&record) {
            crate::chat::runtime::Handle::inspect_remote(&record, proof.clone())
                .map(|inspection| inspection.source)
        } else {
            crate::chat::native_holder::inspect(&record, proof).map(|(recovery, _)| recovery.source)
        }
        .map_err(inspect_failed)?;
        let kind = CardKind::Chat(session);
        for home in std::iter::once(source).chain(target) {
            if uses_home(&kind, home)? {
                return Err(pending_operation(
                    "A detached conversation still uses this account. Reconnect and close it before removing the account.",
                ));
            }
        }
    }
    let directory = holder_registry::directory(Kind::Terminal).map_err(refusal)?;
    for record in holder_registry::observe_account_owners(Kind::Terminal, &proof.boot_id)
        .map_err(|error| inspect_failed(error.to_string()))?
    {
        if resident_terminals.contains(&record.terminal) {
            continue;
        }
        let socket = holder_registry::socket(&directory, record.terminal);
        let (recovery, _) =
            crate::terminal::inspect_holder(&socket, &record, proof).map_err(inspect_failed)?;
        let recipe = recovery
            .execution_recipe
            .as_ref()
            .unwrap_or(&recovery.source.launch);
        let environment = recovery.execution_environment.as_ref();
        super::ensure_credential_roots(
            host,
            &recipe.working_directory,
            environment.map_or(&recipe.environment, |value| &value.0),
            &crate::account::launch_credential_overrides(recipe),
            environment.is_none(),
        )?;
    }
    Ok(())
}

fn require_closed(kind: &CardKind, home: &Home) -> Result<(), ProtocolError> {
    if uses_home(kind, home)? {
        return Err(pending_operation(
            "An open pane still uses this account. Close it before removing the account.",
        ));
    }
    Ok(())
}

fn uses_home(kind: &CardKind, home: &Home) -> Result<bool, ProtocolError> {
    fn recipe_uses_home(
        recipe: &LaunchRecipe,
        providers: &[Provider],
        home: &Home,
    ) -> Result<bool, ProtocolError> {
        for provider in providers {
            let Some(variable) = crate::account::home_variable(*provider) else {
                continue;
            };
            // Saved recipes can predate a captured HOME. An explicit home is
            // still useful evidence. Resident processes are checked separately.
            let selected = recipe
                .environment
                .get(variable)
                .filter(|value| {
                    !recipe.unset_environment.contains(variable)
                        && (*provider == Provider::Codex || !value.is_empty())
                })
                .map(|value| recipe.working_directory.join(value))
                .or_else(|| {
                    let fallback = match provider {
                        Provider::Claude => ".claude",
                        Provider::Codex => ".codex",
                        _ => return None,
                    };
                    recipe
                        .environment
                        .get("HOME")
                        .filter(|value| {
                            !value.is_empty() && !recipe.unset_environment.contains("HOME")
                        })
                        .map(|home| recipe.working_directory.join(home).join(fallback))
                });
            if let Some(path) = selected
                && home.overlaps(&Home::capture(path).map_err(refusal)?)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    let providers = &[Provider::Claude, Provider::Codex];
    match kind {
        CardKind::Chat(session) => {
            if recipe_uses_home(&session.launch.recipe, &[session.launch.provider], home)? {
                return Ok(true);
            }
            if let Some(terminal) = &session.terminal {
                return recipe_uses_home(&terminal.launch, providers, home);
            }
            Ok(false)
        }
        CardKind::Terminal(session) => recipe_uses_home(&session.launch, providers, home),
        _ => Ok(false),
    }
}

pub(in crate::host) fn admit_pane_edit(
    host: &SharedHost,
    edit: &ModelEdit,
) -> Result<(), ProtocolError> {
    let panes = match edit {
        ModelEdit::SeedView { panes, .. } => panes.as_slice(),
        ModelEdit::ChangeView {
            change:
                Change::Insert { pane, .. }
                | Change::InsertIfEmpty { pane }
                | Change::Update { pane, .. },
            ..
        } => std::slice::from_ref(pane),
        _ => return Ok(()),
    };
    let map = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .account_lifecycle
        .clone();
    let homes = map
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .filter(|scope| scope.removing)
        .filter_map(|scope| scope.homes.first().cloned())
        .collect::<Vec<_>>();
    for mut home in homes {
        home.refresh_ancestors().map_err(refusal)?;
        for pane in panes {
            if uses_home(&pane.kind, &home)? {
                return Err(pending_operation(
                    "Wait for the account removal to finish before opening this pane.",
                ));
            }
        }
    }
    Ok(())
}
