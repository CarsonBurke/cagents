//! Authenticated channel intent. Forge reads are separate from the short
//! canonical transactions that decide who may change a relation.
use super::{agent_execution, model::channel_work::TrustedResolvedOrigin, state::SharedHost};
use crate::{
    agent_execution::{Authority, Origin as ExecutionOrigin},
    channel_work::{self as work, Home, Origin, Page, Target},
    channel_work_api::{Caller, Context, Failure, Request, Response},
};

#[cfg(test)]
mod tests;

fn resolve(host: &SharedHost, authority: &Authority) -> Result<TrustedResolvedOrigin, String> {
    let (origin, _) = agent_execution::caller(host, authority)?;
    let registry = agent_execution::registry(host)?;
    let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    match registry.agent_authority_source(authority)? {
        Some(source) => registry.resolve_channel_work_source(authority, &source),
        None if origin == ExecutionOrigin::User && authority.token().is_none() => {
            Ok(TrustedResolvedOrigin {
                origin: Origin::User,
                proof: None,
            })
        }
        None => Err("Channel work needs an authenticated live channel session".into()),
    }
}

fn caller(resolved: &TrustedResolvedOrigin) -> Option<Caller> {
    match (&resolved.origin, &resolved.proof) {
        (Origin::Actor { actor, .. }, Some(proof)) => Some(Caller {
            session: actor.session,
            card: proof.card,
            placement_epoch: proof.placement_epoch,
            run: actor.run,
        }),
        _ => None,
    }
}

pub(super) fn process(host: &SharedHost, authority: &Authority, request: Request) -> Response {
    match process_inner(host, authority, request) {
        Ok(response) => response,
        Err(reason) => Response::Failed(Failure::Refused(reason)),
    }
}

fn process_inner(
    host: &SharedHost,
    authority: &Authority,
    request: Request,
) -> Result<Response, String> {
    let resolved = resolve(host, authority)?;
    let registry = agent_execution::registry(host)?;
    match request {
        Request::Context { channel } => {
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            let model = registry.model()?;
            let selected = match &resolved.origin {
                Origin::Actor { home, .. } => {
                    if channel.is_some_and(|id| id != home.channel) {
                        return Err("A session may declare work only in its own channel".into());
                    }
                    home.channel
                }
                Origin::User => channel.ok_or("Choose a channel explicitly with --channel")?,
            };
            let channel = model
                .channels
                .iter()
                .find(|entry| entry.id == selected)
                .ok_or("The channel no longer exists")?;
            let home = Home {
                registry: model.identity,
                channel: channel.id,
                creation_ordinal: channel.ordinal,
            };
            home.validate().map_err(|error| error.to_string())?;
            Ok(Response::Context(Context {
                home,
                title: channel.title.clone(),
                caller: caller(&resolved),
            }))
        }
        Request::Read { home, page } => {
            let snapshot = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .read_channel_work(&home, page)?;
            Ok(Response::Snapshot(snapshot))
        }
        Request::ReadTarget { home, target } => {
            let target = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .read_channel_work_target(&home, &target)?;
            Ok(Response::Target(target))
        }
        Request::ReadHome { home } => {
            let state = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .read_channel_work_home(&home)?;
            Ok(Response::Home {
                home: state.home,
                lifecycle: state.lifecycle,
                revision: state.revision,
            })
        }
        Request::Receipt { home, operation } => {
            let receipt = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .channel_work_receipt(&resolved, &home, operation)?;
            Ok(Response::Receipt(receipt))
        }
        Request::Mutate {
            mutation,
            caller: captured,
        } => {
            if caller(&resolved) != captured {
                return Err(
                    "The captured session, run or channel placement changed; inspect current work"
                        .into(),
                );
            }
            let result = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .apply_channel_work(&resolved, &mutation);
            match result {
                Ok(reply) => {
                    if reply.receipt.changed {
                        announce(host, mutation.home, reply.receipt.sequence);
                    }
                    Ok(Response::Applied(reply))
                }
                Err(super::model::channel_work::ApplyError::Refused(reason)) => {
                    Ok(Response::Failed(Failure::Refused(reason)))
                }
                Err(super::model::channel_work::ApplyError::CommitUncertain) => {
                    Ok(Response::Failed(Failure::Uncertain))
                }
            }
        }
        Request::Refresh { .. } => Err("Use the targeted forge refresh path".into()),
    }
}

/// Called outside the client's admission guard. Network work never holds the
/// host lock, canonical registry mutex, or global control gate. Every publish
/// reacquires admission, authenticates again and uses the captured entry CAS.
pub(super) fn refresh(host: &SharedHost, authority: &Authority, home: Home) -> Response {
    let mut published = false;
    let result = (|| -> Result<Response, String> {
        let captured = resolve(host, authority)?;
        require_refresh_home(&captured, home)?;
        let captured_caller = caller(&captured);
        let registry = agent_execution::registry(host)?;
        // Candidate selection is independent of display pages: polling a row
        // advances its display sequence and must not starve older relations.
        // The store returns at most four validated rows from the exact home.
        let entries = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .read_channel_work_refresh_candidates(&home)?;
        let budget = RefreshBudget::start()?;
        let github = crate::github::GitHubService::system("gh");
        for entry in entries {
            if budget.cancellation.is_cancelled() {
                break;
            }
            let Target::PullRequest(key) = &entry.target else {
                continue;
            };
            let generation = entry
                .observation
                .as_ref()
                .map_or(0, |observation| observation.generation)
                .checked_add(1)
                .ok_or("The forge observation generation is exhausted")?;
            let outcome = match github.view_with_timeout(
                key,
                &budget.cancellation,
                std::time::Duration::from_secs(3),
            ) {
                Ok(pr) => {
                    // The service validates the exact base identity and PR
                    // URL; mutable head metadata cannot retarget this row.
                    let state = match pr.state.0.as_str() {
                        "OPEN" => work::PrState::Open { draft: pr.is_draft },
                        "CLOSED" => work::PrState::Closed,
                        "MERGED" => work::PrState::Merged,
                        _ => work::PrState::Unknown,
                    };
                    let identity_changed = entry
                        .observation
                        .as_ref()
                        .and_then(|old| old.node_id.as_ref())
                        .zip(pr.id.as_ref())
                        .is_some_and(|(known, current)| known != current);
                    if state == work::PrState::Unknown || identity_changed {
                        // Preserve the trusted node and last known facts, but
                        // make conflict/unavailability durable across reads.
                        work::PollOutcome::Failure
                    } else {
                        work::PollOutcome::Success(observation(generation, state, pr))
                    }
                }
                Err(_) => work::PollOutcome::Failure,
            };
            let gate = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .control_gate
                .clone();
            let _admission = gate.read().unwrap_or_else(|p| p.into_inner());
            super::client::ensure_control_open(host).map_err(|error| error.message)?;
            let current = resolve(host, authority)?;
            require_refresh_home(&current, home)?;
            if caller(&current) != captured_caller {
                return Err(
                    "The channel placement or run changed while PR status was being read".into(),
                );
            }
            let observed = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .observe_channel_work_authenticated(
                    &current,
                    &home,
                    &entry.target,
                    entry.revision,
                    generation,
                    outcome,
                );
            match observed {
                Ok(observed) => {
                    if observed.changed {
                        published = true;
                        announce(host, home, observed.sequence);
                    }
                }
                Err(super::model::channel_work::ApplyError::Refused(reason)) => return Err(reason),
                Err(super::model::channel_work::ApplyError::CommitUncertain) => {
                    return Ok(Response::Failed(Failure::Uncertain));
                }
            }
        }
        let snapshot = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .read_channel_work(&home, Page::default())?;
        Ok(Response::Snapshot(snapshot))
    })();
    result.unwrap_or_else(|reason| {
        Response::Failed(if published {
            Failure::Uncertain
        } else {
            Failure::Refused(reason)
        })
    })
}

fn announce(host: &SharedHost, home: Home, sequence: u64) {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .announce_model_edit(
            0,
            sequence,
            &crate::workspace_model::ModelEdit::ChannelWorkChanged { home },
        );
}

/// One finite timer owns cancellation for the whole refresh, including permit
/// waiting. Normal completion wakes and joins it immediately; it does not leave
/// an idle timeout thread behind each request.
struct RefreshBudget {
    cancellation: crate::repository::CancellationToken,
    finished: std::sync::mpsc::Sender<()>,
    timer: Option<std::thread::JoinHandle<()>>,
}
impl RefreshBudget {
    fn start() -> Result<Self, String> {
        let cancellation = crate::repository::CancellationToken::new();
        let timed = cancellation.clone();
        let (finished, wait) = std::sync::mpsc::channel();
        let timer = std::thread::Builder::new()
            .name("cagents-channel-forge-budget".into())
            .spawn(move || {
                if matches!(
                    wait.recv_timeout(std::time::Duration::from_secs(12)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    timed.cancel();
                }
            })
            .map_err(|_| "The forge refresh budget is unavailable")?;
        Ok(Self {
            cancellation,
            finished,
            timer: Some(timer),
        })
    }
}
impl Drop for RefreshBudget {
    fn drop(&mut self) {
        let _ = self.finished.send(());
        if let Some(timer) = self.timer.take() {
            let _ = timer.join();
        }
    }
}

fn require_refresh_home(resolved: &TrustedResolvedOrigin, home: Home) -> Result<(), String> {
    if matches!(&resolved.origin, Origin::Actor { home: actual, .. } if *actual != home) {
        Err("A session may refresh work only in its captured channel".into())
    } else {
        Ok(())
    }
}

fn observation(
    generation: u64,
    state: work::PrState,
    pr: crate::github::PullRequest,
) -> work::Observation {
    fn bounded(value: &str, limit: usize) -> bool {
        !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
    }
    // Optional presentation metadata must not make a valid status unusable.
    // Omit an oversized or malformed field; retain the exact tracked PR key.
    let head = pr
        .head_repository_key()
        .filter(|_| {
            bounded(&pr.head_ref_name, 1024)
                && (7..=64).contains(&pr.head_ref_oid.len())
                && pr.head_ref_oid.bytes().all(|b| b.is_ascii_hexdigit())
        })
        .map(|repository| work::Head {
            repository,
            branch: pr.head_ref_name.clone(),
            oid: pr.head_ref_oid.clone(),
        });
    work::Observation {
        generation,
        state,
        freshness: work::Freshness::Current,
        title: bounded(&pr.title, 512).then_some(pr.title),
        node_id: pr.id.filter(|id| bounded(id, 256)),
        head,
        observed_at_ms: crate::chat::now_ms().unwrap_or(0),
    }
}
