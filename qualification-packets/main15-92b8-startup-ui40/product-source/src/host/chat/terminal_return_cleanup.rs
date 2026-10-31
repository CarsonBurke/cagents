//! A return response grants cleanup of one original worker, never its reusable ID.
use super::*;
use crate::terminal_ipc::TerminalCloseResult;

const MAX_ISSUED_RETURNS: usize = 1024;
// Settled ACKs retain no process handles. After this many later successful
// ACKs an old lost-reply retry may explicitly refuse; it never selects an ID.
const MAX_ACKNOWLEDGED_RETURNS: usize = 1024;

#[derive(Clone)]
pub(in crate::host) struct Receipt {
    terminal: crate::model::TerminalSession,
    proof: HostSessionProof,
    worker: crate::terminal::TerminalHandle,
    native: Option<CloseTarget>,
    closed: bool,
    closing: bool,
}

impl Receipt {
    pub(in crate::host) fn pending(&self) -> bool {
        !self.closed
    }

    pub(in crate::host) fn terminal_id(&self) -> TerminalId {
        self.terminal.id
    }

    /// A checkpoint carries hosted owners. Preserve a private unsettled owner
    /// that those rows can no longer account for, without extending normal
    /// handoff grace for an owner that is still hosted.
    pub(in crate::host) fn blocks_upgrade(&self, state: &HostState) -> bool {
        self.pending() && (self.closing || self.require_current(state).is_err())
    }

    pub(in crate::host) fn working_directories(&self) -> Vec<&std::path::Path> {
        let mut directories = vec![self.terminal.launch.working_directory.as_path()];
        if let Some(native) = &self.native {
            directories.push(native.source.launch.recipe.working_directory.as_path());
        }
        directories
    }

    fn same_owners(&self, other: &Self) -> bool {
        self.proof == other.proof
            && self.worker.same_worker(&other.worker)
            && self.terminal.id == other.terminal.id
            && matches_paired_terminal(&self.terminal, &other.terminal, &self.proof)
            && match (&self.native, &other.native) {
                (None, None) => true,
                (Some(left), Some(right)) => {
                    left.source == right.source
                        && left.instance == right.instance
                        && left.handle.same_attachment(&right.handle)
                }
                _ => false,
            }
    }

    fn require_current(&self, state: &HostState) -> Result<(), ProtocolError> {
        if state.shutting_down || state.proof() != &self.proof {
            return Err(unavailable(
                "The returned terminal's original daemon is unavailable",
            ));
        }
        if !state
            .terminals
            .get(&self.terminal.id)
            .is_some_and(|record| {
                record.handle.same_worker(&self.worker)
                    && matches_paired_terminal(&record.session, &self.terminal, &self.proof)
            })
        {
            return Err(unavailable(
                "The returned terminal's original worker is unavailable or changed; its replacement was preserved",
            ));
        }
        match &self.native {
            Some(target)
                if state.chats.get(&target.source.id).is_some_and(|record| {
                    target.matches(record)
                        && record
                            .paired_terminal
                            .as_ref()
                            .is_some_and(|worker| worker.same_worker(&self.worker))
                        && record.session.terminal.as_ref().is_some_and(|source| {
                            matches_paired_terminal(&self.terminal, source, &self.proof)
                        })
                }) =>
            {
                Ok(())
            }
            None if !state.chats.contains_key(&self.terminal.id) => Ok(()),
            _ => Err(unavailable(
                "The returned terminal's original native owner changed; its replacement was preserved",
            )),
        }
    }
}

/// Reserve a bounded capability slot before a return stops or starts owners.
/// Replaying the same live Codex pair needs no second slot at the capacity limit.
pub(super) struct Reservation {
    host: SharedHost,
    token: uuid::Uuid,
    reused: bool,
}

impl Reservation {
    pub(super) fn new(
        host: &SharedHost,
        state: &mut HostState,
        chat_id: TerminalId,
    ) -> Result<Self, ProtocolError> {
        if let Some(record) = state.chats.get(&chat_id)
            && let Some(worker) = &record.paired_terminal
            && !worker.is_completed()
            && let Some((&token, _)) = state.issued_terminal_returns.iter().find(|(_, receipt)| {
                !receipt.closed
                    && !receipt.closing
                    && receipt.worker.same_worker(worker)
                    && receipt
                        .native
                        .as_ref()
                        .is_some_and(|target| target.matches(record))
                    && receipt.require_current(state).is_ok()
            })
        {
            return Ok(Self {
                host: host.clone(),
                token,
                reused: true,
            });
        }
        if state.issued_terminal_returns.len() + state.terminal_return_reservations.len()
            >= MAX_ISSUED_RETURNS
        {
            return Err(unavailable(
                "Too many retained terminal cleanup receipts; save or explicitly finish existing returns before converting another chat",
            ));
        }
        let token = loop {
            let token = uuid::Uuid::new_v4();
            if !state.issued_terminal_returns.contains_key(&token)
                && !state.terminal_return_reservations.contains(&token)
                && !state
                    .acknowledged_terminal_returns
                    .iter()
                    .any(|(old, _, _)| *old == token)
            {
                break token;
            }
        };
        state.terminal_return_reservations.insert(token);
        Ok(Self {
            host: host.clone(),
            token,
            reused: false,
        })
    }

    /// Caller holds the final publication lock and has validated its source.
    pub(super) fn publish(
        &self,
        state: &mut HostState,
        terminal: &crate::model::TerminalSession,
        worker: &crate::terminal::TerminalHandle,
        native: Option<CloseTarget>,
    ) -> Result<uuid::Uuid, ProtocolError> {
        let receipt = Receipt {
            terminal: terminal.clone(),
            proof: state.proof().clone(),
            worker: worker.clone(),
            native,
            closed: false,
            closing: false,
        };
        receipt.require_current(state)?;
        if let Some((&token, _)) = state
            .issued_terminal_returns
            .iter()
            .find(|(_, old)| !old.closed && !old.closing && old.same_owners(&receipt))
        {
            return Ok(token);
        }
        if self.reused || !state.terminal_return_reservations.contains(&self.token) {
            return Err(unavailable(
                "The exact terminal cleanup admission changed before publication",
            ));
        }
        state.issued_terminal_returns.insert(self.token, receipt);
        state.terminal_return_reservations.remove(&self.token);
        state.inventory_generation += 1;
        Ok(self.token)
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .terminal_return_reservations
            .remove(&self.token);
    }
}

/// Unwinding a host request ends its active attempt, not its original custody.
/// Clear only this same pending capability; neither reacquire nor retry owners.
struct Attempt {
    host: SharedHost,
    token: uuid::Uuid,
    receipt: Receipt,
}

impl Drop for Attempt {
    fn drop(&mut self) {
        let mut state = self.host.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(current) = state.issued_terminal_returns.get_mut(&self.token)
            && current.same_owners(&self.receipt)
            && !current.closed
        {
            current.closing = false;
        }
    }
}

pub(super) fn replay(
    state: &HostState,
    terminal: &crate::model::TerminalSession,
    token: uuid::Uuid,
) -> Result<(), ProtocolError> {
    let receipt = state.issued_terminal_returns.get(&token).ok_or_else(|| {
        unavailable(
            "The original terminal cleanup receipt is unavailable; no replacement was selected",
        )
    })?;
    if receipt.terminal.id != terminal.id || receipt.closed || receipt.closing {
        return Err(unavailable(
            "This terminal return no longer has an available original cleanup owner",
        ));
    }
    receipt.require_current(state)
}

/// Legacy ACKs can retire generic replay metadata, but never physical authority.
/// A stale explicit nonce cannot consume a newer worker's receipt or replay row.
pub(super) fn acknowledge(
    state: &mut HostState,
    id: TerminalId,
    token: Option<uuid::Uuid>,
) -> Result<Option<Receipt>, ProtocolError> {
    if let Some(token) = token {
        if token.is_nil() {
            return Err(unavailable(
                "The original terminal cleanup receipt is missing",
            ));
        }
        let Some(receipt) = state
            .issued_terminal_returns
            .get(&token)
            .filter(|receipt| receipt.terminal.id == id)
        else {
            return if state
                .acknowledged_terminal_returns
                .iter()
                .any(|(old, terminal, proof)| {
                    *old == token && *terminal == id && proof == state.proof()
                }) {
                Ok(None)
            } else {
                Err(unavailable(
                    "The exact terminal ACK receipt is unavailable; no cleanup authority or replay row was changed",
                ))
            };
        };
        if receipt.closing {
            return Err(unavailable(
                "The original terminal cleanup is still in progress; its receipt was retained",
            ));
        }
        // Durable publication transfers cleanup back to a hosted original
        // record. A missing/changed owner has no such custodian, so ACK cannot
        // discard its last private handle or tell the GUI to forget its nonce.
        if !receipt.closed {
            receipt.require_current(state)?;
        }
        let receipt = state
            .issued_terminal_returns
            .remove(&token)
            .expect("checked receipt");
        if state.acknowledged_terminal_returns.len() >= MAX_ACKNOWLEDGED_RETURNS {
            state.acknowledged_terminal_returns.pop_front();
        }
        state
            .acknowledged_terminal_returns
            .push_back((token, id, receipt.proof.clone()));
        let source = state.chat_handoffs.iter().find_map(|(&source, handoff)| {
            (handoff.terminal.id == id && handoff.cleanup_receipt == Some(token)).then_some(source)
        });
        if let Some(source) = source {
            state.chat_handoffs.remove(&source);
        }
        state.inventory_generation += 1;
        // The caller must release HostState before dropping the last original
        // worker/native handle: their destructors can join and checkpoint.
        return Ok(Some(receipt));
    } else if let Some(source) = state
        .chat_handoffs
        .iter()
        .find_map(|(&source, handoff)| (handoff.terminal.id == id).then_some(source))
    {
        state.chat_handoffs.remove(&source);
        state.inventory_generation += 1;
    }
    Ok(None)
}

/// Endpoint caller holds the exclusive control gate and authenticates User.
/// Every physical operation below uses the captured private owner. No Attach,
/// inventory lookup by public descriptor, or close_for_terminal is involved.
pub(in crate::host) fn close(
    host: &SharedHost,
    terminal_id: TerminalId,
    token: uuid::Uuid,
) -> Result<TerminalCloseResult, ProtocolError> {
    if token.is_nil() {
        return Err(unavailable(
            "The original terminal cleanup receipt is missing",
        ));
    }
    let (receipt, native) = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let receipt = state.issued_terminal_returns.get(&token)
            .filter(|receipt| receipt.terminal.id == terminal_id)
            .cloned().ok_or_else(|| unavailable(
                "The original terminal cleanup receipt is unavailable; no replacement was selected",
            ))?;
        if receipt.proof != *state.proof() {
            return Err(unavailable("The original terminal cleanup daemon changed"));
        }
        if receipt.closed {
            // Positive original close is cached before its reply; later IDs
            // are deliberately not inspected or touched by this replay.
            return Ok(TerminalCloseResult::Closed);
        }
        if receipt.closing {
            return Err(unavailable(
                "The original terminal cleanup is already in progress",
            ));
        }
        receipt.require_current(&state)?;
        let native = if let Some(target) = &receipt.native {
            let owners = paired_close::Owners::capture(&state, &state.chats[&target.source.id])?;
            let closing = capacity::Reservation::closing(host, &mut state, &target.source)?;
            Some((owners, closing))
        } else {
            None
        };
        state
            .issued_terminal_returns
            .get_mut(&token)
            .expect("checked receipt")
            .closing = true;
        (receipt, native)
    };
    let _attempt = Attempt {
        host: host.clone(),
        token,
        receipt: receipt.clone(),
    };
    let outcome = (|| {
        if let Some((owners, closing)) = &native {
            super::super::agent_execution::preflight_retirement(host, &owners.target.source)
                .map_err(unavailable)?;
            owners.close(host, closing)?;
            owners.require_current(host, closing)?;
            super::super::agent_execution::retired(host, &owners.target.source)
                .map_err(unavailable)?;
        } else if receipt.worker.holder_identity().is_some() {
            receipt
                .worker
                .close_with_receipt(&receipt.terminal, &receipt.proof)
                .map_err(unavailable)?;
        } else {
            // A local worker's successful synchronous shutdown is an actual
            // original-owner release, rather than an absent/completed lookup.
            receipt
                .worker
                .shutdown()
                .map_err(|error| unavailable(error.to_string()))?;
        }
        let removed = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            receipt.require_current(&state)?;
            if !state
                .issued_terminal_returns
                .get(&token)
                .is_some_and(|current| current.closing && current.same_owners(&receipt))
            {
                return Err(unavailable(
                    "The original terminal cleanup capability changed during close",
                ));
            }
            let removed = if let Some((owners, closing)) = &native {
                owners.require_locked(&state, closing)?;
                state.chats.remove(&owners.target.source.id)
            } else {
                None
            };
            // Commit the positive proof before withdrawing its original
            // records, then keep it until an exact durable ACK consumes it.
            let current = state
                .issued_terminal_returns
                .get_mut(&token)
                .expect("checked receipt");
            current.closed = true;
            current.closing = false;
            state.close_terminal_attention(terminal_id, Some(&receipt.worker));
            state.retire_terminal(terminal_id);
            state.inventory_generation += 1;
            removed
        };
        drop(removed);
        if let Some(target) = &receipt.native {
            super::super::state::retire_recovered_upgrade_chat(host, target.source.id);
        }
        Ok(TerminalCloseResult::Closed)
    })();
    outcome
}

#[cfg(test)]
mod tests;
