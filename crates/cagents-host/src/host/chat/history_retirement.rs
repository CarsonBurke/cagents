//! Release recoverable paging actors once no pane, subscription or pending resolution needs them.
use super::*;
use std::sync::mpsc::RecvTimeoutError;

pub(in crate::host) fn start(host: &SharedHost) -> std::io::Result<()> {
    let weak = Arc::downgrade(host);
    let (signal, pending) = std::sync::mpsc::sync_channel(1);
    thread::Builder::new()
        .name("cagents-history-retirement".into())
        .spawn(move || {
            let mut deadline: Option<Instant> = None;
            while match deadline {
                Some(expiry) => !matches!(
                    pending.recv_timeout(expiry.saturating_duration_since(Instant::now())),
                    Err(RecvTimeoutError::Disconnected)
                ),
                None => pending.recv().is_ok(),
            } {
                let Some(host) = weak.upgrade() else {
                    return;
                };
                // Take the deadline before collecting, so a lease that expires
                // mid-pass always triggers another reference check. Expiry
                // removes only the pending Resolve reference.
                let now = Instant::now();
                deadline = host
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .chat_resolutions
                    .values()
                    .filter_map(upgrade_resolution::Attempt::history_expiry)
                    .filter(|expiry| *expiry > now)
                    .min();
                if let Err(error) = collect(&host) {
                    tracing::debug!(%error, "Unreferenced history readers were kept");
                }
            }
        })?;
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    state.history_retirement = Some(signal);
    state.recheck_history_readers();
    Ok(())
}

pub(super) fn collect(host: &SharedHost) -> Result<(), String> {
    let (candidates, model_path, gate) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down || state.startup_state() != crate::terminal_ipc::StartupState::Ready
        {
            return Ok(());
        }
        let candidates: Vec<_> = state
            .chats
            .iter()
            .filter(|(id, record)| {
                record.unsubscribed_history()
                    && !state.window_claims_session(**id)
                    && !capacity::history_resolution_pending(&state, record)
            })
            .map(|(&id, record)| (id, record.execution_instance))
            .collect();
        if candidates.is_empty() {
            return Ok(());
        }
        let Some(model_path) = state.model_path() else {
            return Ok(());
        };
        (
            candidates,
            model_path.to_owned(),
            state.control_gate.clone(),
        )
    };
    // Legacy window inventories can take time to read. They and the first
    // transcript coverage check run without holding exclusive admission.
    let model = crate::host::model::Registry::at(&model_path)?;
    let sequence = model.saved_session_inventory_sequence()?;
    let saved = model.saved_session_inventory()?;
    for (id, execution) in candidates {
        if saved.contains(&id) {
            continue;
        }
        let Ok(inspection) = capacity::inspect(host, id) else {
            continue;
        };
        if inspection.proof.execution_instance != execution || inspection.release_blocked.is_some()
        {
            continue;
        }
        let release = {
            // Recheck pane membership and reserve the exact paging worker
            // before another request can claim it.
            let _exclusive = gate.exclusive_when_free();
            if model.saved_session_inventory_sequence()? != sequence {
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .recheck_history_readers();
                return Ok(());
            }
            let still_eligible = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                state.startup_state() == crate::terminal_ipc::StartupState::Ready
                    && !state.window_claims_session(id)
                    && state.chats.get(&id).is_some_and(|record| {
                        record.unsubscribed_history() && record.execution_instance == execution
                    })
            };
            if !still_eligible {
                continue;
            }
            capacity::PendingRelease::reserve(host, &inspection)
        };
        let retired = release.and_then(|release| {
            // The allocation fence excludes Attach and Resolve. Hold shared
            // admission for account stability while loading and joining.
            let closed = {
                let _shared = gate.shared();
                release.close_worker(host)?
            };
            let _exclusive = gate.exclusive_when_free();
            closed.finish(host)
        });
        if let Err(error) = retired {
            tracing::debug!(chat = %id, error = %error.message, "History reader release was deferred");
        }
    }
    Ok(())
}
