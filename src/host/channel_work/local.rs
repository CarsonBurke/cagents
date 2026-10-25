//! Local declarations use a daemon-captured checkout. No caller can turn a
//! pathname, current working directory or guessed UUID into native authority.
use super::{RefreshBudget, announce, caller, contexts, resolve};
use crate::{
    agent_execution::{Authority, DirectoryIdentity, HostedSession},
    channel_work::{Change, Home, HomeLifecycle, Mutation, Origin, Target},
    channel_work_api::{Caller, Failure, LocalSelection, Response},
    host::{
        agent_execution,
        model::channel_work::{ApplyError, TrustedResolvedOrigin},
        state::SharedHost,
    },
};
use std::{fs::File, path::PathBuf, sync::Arc, time::Instant};
use uuid::Uuid;

#[cfg(test)]
mod tests;

fn owner(resolved: &TrustedResolvedOrigin) -> contexts::Owner {
    contexts::Owner {
        source: resolved.origin.source(),
        caller: caller(resolved),
        native: resolved.proof.as_ref().map(|proof| proof.source.clone()),
    }
}

fn captured_matches(
    resolved: &TrustedResolvedOrigin,
    expected: &Option<Caller>,
) -> Result<(), String> {
    if &caller(resolved) == expected {
        Ok(())
    } else {
        Err("The captured session, run or channel placement changed; inspect current work".into())
    }
}

fn require_user(resolved: &TrustedResolvedOrigin) -> Result<(), String> {
    if matches!(&resolved.origin, Origin::User) && resolved.proof.is_none() {
        Ok(())
    } else {
        Err(
            "A native session must inspect its own original launch directory; use track-own-local"
                .into(),
        )
    }
}

fn require_actor(resolved: &TrustedResolvedOrigin, home: Home) -> Result<(), String> {
    match (&resolved.origin, &resolved.proof) {
        (
            Origin::Actor {
                home: actual,
                actor,
                ..
            },
            Some(proof),
        ) if *actual == home
            && proof.source.id() == actor.session
            && proof.source.has_complete_proof() =>
        {
            Ok(())
        }
        _ => Err("Own-launch selection needs a live native session in its captured channel".into()),
    }
}

fn require_selected_source(resolved: &TrustedResolvedOrigin, home: Home) -> Result<(), String> {
    match &resolved.origin {
        Origin::User => require_user(resolved),
        Origin::Actor { .. } => require_actor(resolved, home),
        Origin::SelectedUser { .. } => Err("The original selection source is unavailable".into()),
    }
}

enum LaunchHandle {
    Chat(crate::chat::runtime::Handle),
    Terminal(crate::terminal::TerminalHandle),
}

fn original_directory(
    host: &SharedHost,
    resolved: &TrustedResolvedOrigin,
) -> Result<(DirectoryIdentity, Arc<File>), String> {
    let source = &resolved
        .proof
        .as_ref()
        .ok_or("The original native source is unavailable")?
        .source;
    let handle = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        match source {
            HostedSession::Chat(session) => LaunchHandle::Chat(
                state
                    .chats
                    .get(&session.id)
                    .filter(|record| {
                        !record.handle.is_completed()
                            && source.same_process(&HostedSession::Chat(record.session.clone()))
                    })
                    .map(|record| record.handle.clone())
                    .ok_or("The exact native provider is no longer live")?,
            ),
            HostedSession::Terminal(session) => {
                // Stored terminal recipes need not carry their current live
                // continuity. Compare the issued source with the daemon's
                // authenticated live projection, as ordinary caller admission does.
                let current = state.native_terminal_projection(session.id)?;
                if !source.same_process(&HostedSession::Terminal(current)) {
                    return Err("The exact native terminal is no longer live".into());
                }
                LaunchHandle::Terminal(
                    state
                        .terminals
                        .get(&session.id)
                        .map(|record| record.handle.clone())
                        .ok_or("The exact native terminal is no longer live")?,
                )
            }
        }
    };
    // A retained provider may perform a bounded holder request. Do not retain
    // the host or control gate across that request, or reopen its recipe path.
    match handle {
        LaunchHandle::Chat(handle) => handle.pinned_launch_directory(),
        LaunchHandle::Terminal(handle) => handle.pinned_launch_directory(),
    }
}

fn verify_launch(
    host: &SharedHost,
    resolved: &TrustedResolvedOrigin,
    home: Home,
    pin: &contexts::capture::CapturedCheckout,
) -> Result<(), String> {
    if matches!(&resolved.origin, Origin::Actor { .. }) {
        require_actor(resolved, home)?;
        let (identity, file) = original_directory(host, resolved)?;
        if identity != pin.directory_identity()
            || DirectoryIdentity::from_metadata(
                &file
                    .metadata()
                    .map_err(|_| "The original launch directory could not be checked")?,
            ) != identity
        {
            return Err("The native session's original launch directory changed".into());
        }
    }
    Ok(())
}

enum SelectionScope {
    Directory(PathBuf),
    OwnLaunch,
}

impl SelectionScope {
    fn require(&self, resolved: &TrustedResolvedOrigin, home: Home) -> Result<(), String> {
        match self {
            Self::Directory(directory) => {
                require_user(resolved)?;
                if !directory.is_absolute()
                    || directory.as_os_str().as_encoded_bytes().len()
                        > crate::channel_work::MAX_CONTEXT_PATH_BYTES
                    || directory.as_os_str().as_encoded_bytes().contains(&0)
                    || directory
                        .components()
                        .any(|part| matches!(part, std::path::Component::ParentDir))
                {
                    return Err("Choose an absolute checkout path without parent traversal, within the supported path length".into());
                }
                Ok(())
            }
            Self::OwnLaunch => require_actor(resolved, home),
        }
    }
}

pub(super) fn prepare(
    host: &SharedHost,
    authority: &Authority,
    home: Home,
    directory: PathBuf,
    captured: Option<Caller>,
) -> Response {
    prepare_inner(
        host,
        authority,
        home,
        SelectionScope::Directory(directory),
        captured,
    )
}

pub(super) fn prepare_own(
    host: &SharedHost,
    authority: &Authority,
    home: Home,
    captured: Option<Caller>,
) -> Response {
    prepare_inner(host, authority, home, SelectionScope::OwnLaunch, captured)
}

fn prepare_inner(
    host: &SharedHost,
    authority: &Authority,
    home: Home,
    scope: SelectionScope,
    captured: Option<Caller>,
) -> Response {
    let result = (|| -> Result<LocalSelection, String> {
        let initial = resolve(host, authority)?;
        captured_matches(&initial, &captured)?;
        scope.require(&initial, home)?;
        let expected_owner = owner(&initial);
        let registry = agent_execution::registry(host)?;
        let repository = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .read_channel_work_repository(&home)?;
        let _inspection = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .channel_work_selections
            .inspect()?;
        let budget = RefreshBudget::start()?;
        // All Git/FS work runs without host, registry or control-gate locks.
        let pin = Arc::new(match &scope {
            SelectionScope::Directory(directory) => {
                contexts::capture::CapturedCheckout::capture_user(
                    &repository,
                    directory,
                    &budget.cancellation,
                )?
            }
            SelectionScope::OwnLaunch => {
                let (identity, original) = original_directory(host, &initial)?;
                contexts::capture::CapturedCheckout::capture_actor(
                    &repository,
                    &identity,
                    original,
                    &budget.cancellation,
                )?
            }
        });
        if matches!(&scope, SelectionScope::OwnLaunch) {
            let inspected = resolve(host, authority)?;
            captured_matches(&inspected, &captured)?;
            scope.require(&inspected, home)?;
            if owner(&inspected) != expected_owner {
                return Err("The native source changed during inspection".into());
            }
            verify_launch(host, &inspected, home, &pin)?;
        }
        let gate = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .control_gate
            .clone();
        let _admission = gate.read().unwrap_or_else(|p| p.into_inner());
        super::super::client::ensure_control_open(host).map_err(|error| error.message)?;
        let current = resolve(host, authority)?;
        captured_matches(&current, &captured)?;
        scope.require(&current, home)?;
        if owner(&current) != expected_owner
            || registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .read_channel_work_repository(&home)?
                != repository
        {
            return Err(
                "The selected channel or project changed; inspect the checkout again".into(),
            );
        }
        let candidate = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .channel_work_selections
            .issue(home, expected_owner.clone(), pin, Instant::now())?;
        let point = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .read_channel_work_target(&home, &Target::Local(candidate.context.id));
        let point = match point {
            Ok(point) if point.lifecycle == HomeLifecycle::Active => point,
            _ => {
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .channel_work_selections
                    .consume(home, &expected_owner, candidate.selection);
                return Err(
                    "The local work target is unavailable; inspect the checkout again".into(),
                );
            }
        };
        let selected = LocalSelection {
            selection: candidate.selection,
            home: candidate.home,
            context: candidate.context,
            expected_revision: point.entry.as_ref().map_or(0, |entry| entry.revision),
        };
        selected.validate().map_err(|error| error.to_string())?;
        Ok(selected)
    })();
    match result {
        Ok(selection) => Response::Selected(selection),
        Err(reason) => Response::Failed(Failure::Refused(reason)),
    }
}

pub(super) fn apply(
    host: &SharedHost,
    authority: &Authority,
    selection: Uuid,
    mutation: Mutation,
    captured: Option<Caller>,
) -> Response {
    let mut reconcile = false;
    let result = (|| -> Result<crate::channel_work::Reply, ApplyError> {
        let mutation = mutation
            .canonicalized()
            .map_err(|error| error.to_string())?;
        if selection.is_nil()
            || !matches!(
                &mutation.change,
                Change::Claim {
                    target: Target::Local(_),
                    ..
                }
            )
        {
            return Err("A checkout selection can only claim its selected local work".into());
        }
        let gate = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .control_gate
            .clone();
        let expected_owner = {
            let _admission = gate.read().unwrap_or_else(|p| p.into_inner());
            super::super::client::ensure_control_open(host).map_err(|error| error.message)?;
            let current = resolve(host, authority)?;
            captured_matches(&current, &captured)?;
            let registry = agent_execution::registry(host)?;
            let replay = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .try_replay_channel_work(&current, &mutation)?;
            // Expiry, consumption and daemon restart cannot erase an exact
            // original operation. Missing never admits a fresh write.
            if let Some(reply) = replay {
                return Ok(reply);
            }
            reconcile = true;
            require_selected_source(&current, mutation.home)?;
            owner(&current)
        };
        let attempt = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .channel_work_selections
            .begin_operation(
                mutation.home,
                &expected_owner,
                selection,
                &mutation,
                Instant::now(),
            )
            .map_err(|error| match error {
                contexts::BeginError::Refused(reason) => ApplyError::Refused(reason),
                contexts::BeginError::InFlight => ApplyError::CommitUncertain,
            })?;
        let prepared = &attempt.prepared;
        let before_read = resolve(host, authority)?;
        captured_matches(&before_read, &captured)?;
        require_selected_source(&before_read, mutation.home)?;
        if owner(&before_read) != expected_owner {
            return Err("The selected caller changed before checkout inspection".into());
        }
        verify_launch(host, &before_read, mutation.home, &prepared.pin)?;
        let budget = RefreshBudget::start()?;
        let _inspection = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .channel_work_selections
            .inspect()?;
        // Preserve the original selection's point-in-time observations. This
        // read verifies physical/branch binding; it must not silently replace
        // selected provenance with newer dirty or commit-head observations.
        let _observed = prepared.pin.revalidate(&budget.cancellation)?;
        let inspected = resolve(host, authority)?;
        captured_matches(&inspected, &captured)?;
        require_selected_source(&inspected, mutation.home)?;
        if owner(&inspected) != expected_owner {
            return Err("The selected caller changed during checkout inspection".into());
        }
        verify_launch(host, &inspected, mutation.home, &prepared.pin)?;
        let _admission = gate.read().unwrap_or_else(|p| p.into_inner());
        super::super::client::ensure_control_open(host).map_err(|error| error.message)?;
        let mut current = resolve(host, authority)?;
        captured_matches(&current, &captured)?;
        require_selected_source(&current, mutation.home)?;
        if owner(&current) != expected_owner {
            return Err("The selected caller changed; inspect the checkout again".into());
        }
        // The original attempt remains exclusive through commit. Recheck its
        // expiry and exact owner after I/O, without holding the host lock while
        // waiting for the registry transaction.
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .channel_work_selections
            .confirm_operation(
                mutation.home,
                &expected_owner,
                selection,
                &attempt,
                Instant::now(),
            )?;
        current.origin = match current.origin {
            Origin::User => Origin::SelectedUser {
                home: mutation.home,
                contexts: vec![prepared.context.clone()],
            },
            Origin::Actor { home, actor, .. } => Origin::Actor {
                home,
                actor,
                contexts: vec![prepared.context.clone()],
            },
            Origin::SelectedUser { .. } => {
                return Err("The original selection source is unavailable".into());
            }
        };
        let registry = agent_execution::registry(host)?;
        let reply = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .apply_channel_work(&current, &mutation)?;
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .channel_work_selections
            .consume(mutation.home, &expected_owner, selection);
        Ok(reply)
    })();
    // Another identical request may have committed and consumed the selection
    // between our initial receipt miss and a later refusal. Reconcile the
    // original operation once; never turn a missing receipt into a fresh write.
    let result = if reconcile && result.is_err() {
        reconcile_result(host, authority, &mutation, &captured, result)
    } else {
        result
    };
    match result {
        Ok(reply) => {
            if reply.receipt.changed {
                announce(host, mutation.home, reply.receipt.sequence);
            }
            Response::Applied(reply)
        }
        Err(ApplyError::Refused(reason)) => Response::Failed(Failure::Refused(reason)),
        Err(ApplyError::CommitUncertain) => Response::Failed(Failure::Uncertain),
    }
}

fn reconcile_result(
    host: &SharedHost,
    authority: &Authority,
    mutation: &Mutation,
    captured: &Option<Caller>,
    original: Result<crate::channel_work::Reply, ApplyError>,
) -> Result<crate::channel_work::Reply, ApplyError> {
    let replay = (|| -> Result<Option<crate::channel_work::Reply>, ApplyError> {
        let gate = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .control_gate
            .clone();
        let _admission = gate.read().unwrap_or_else(|p| p.into_inner());
        super::super::client::ensure_control_open(host).map_err(|error| error.message)?;
        let current = resolve(host, authority)?;
        captured_matches(&current, captured)?;
        let registry = agent_execution::registry(host)?;
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .try_replay_channel_work(&current, mutation)
    })();
    match replay {
        Ok(Some(reply)) => Ok(reply),
        Ok(None) => original,
        // Admission or receipt-read failure cannot prove the original outcome.
        Err(_) => Err(ApplyError::CommitUncertain),
    }
}
