//! Capture every hosted owner before stopping either side of a native pair.
use super::*;

pub(super) struct Owners {
    pub(super) target: CloseTarget,
    proof: HostSessionProof,
    paired: Option<(
        crate::model::TerminalSession,
        crate::terminal::TerminalHandle,
    )>,
}

impl Owners {
    pub(super) fn capture(state: &HostState, record: &Record) -> Result<Self, ProtocolError> {
        let target = CloseTarget::capture(state, record)?;
        let paired = match (&record.session.terminal, &record.paired_terminal) {
            (None, None) => None,
            (Some(source), Some(worker)) => {
                if !state.terminals.get(&source.id).is_some_and(|current| {
                    current.handle.same_worker(worker)
                        && matches_paired_terminal(&current.session, source, state.proof())
                }) {
                    return Err(unavailable(
                        "The original paired terminal changed; its replacement was preserved",
                    ));
                }
                if record.handle.actor_identity().is_some() && worker.holder_identity().is_none() {
                    return Err(unavailable(
                        "The retained actor's paired terminal lacks authenticated cleanup ownership",
                    ));
                }
                Some((source.clone(), worker.clone()))
            }
            _ => {
                return Err(unavailable(
                    "Native pairing lacks its exact terminal worker evidence",
                ));
            }
        };
        Ok(Self {
            target,
            proof: state.proof().clone(),
            paired,
        })
    }

    pub(super) fn require_locked(
        &self,
        state: &HostState,
        closing: &capacity::Reservation,
    ) -> Result<(), ProtocolError> {
        if state.shutting_down
            || state.proof() != &self.proof
            || !closing.holds(state, self.target.source.id)
            || state.chat_handoffs.contains_key(&self.target.source.id)
            || !state
                .chats
                .get(&self.target.source.id)
                .is_some_and(|record| {
                    self.target.matches(record)
                        && match (&self.paired, &record.paired_terminal) {
                            (None, None) => true,
                            (Some((_, expected)), Some(current)) => current.same_worker(expected),
                            _ => false,
                        }
                })
            || self.paired.as_ref().is_some_and(|(source, worker)| {
                !state.terminals.get(&source.id).is_some_and(|current| {
                    current.handle.same_worker(worker)
                        && matches_paired_terminal(&current.session, source, &self.proof)
                })
            })
        {
            return Err(unavailable(
                "The exact native attachment or paired worker changed during teardown",
            ));
        }
        Ok(())
    }

    pub(super) fn require_current(
        &self,
        host: &SharedHost,
        closing: &capacity::Reservation,
    ) -> Result<(), ProtocolError> {
        self.require_locked(&host.lock().unwrap_or_else(|p| p.into_inner()), closing)
    }

    /// Preserve both records for retry until the caller commits its own outcome.
    /// In particular, returning to terminal must succeed the lineage, not close it.
    pub(super) fn close(
        &self,
        host: &SharedHost,
        closing: &capacity::Reservation,
    ) -> Result<(), ProtocolError> {
        self.require_current(host, closing)?;
        self.target.handle.close().map_err(unavailable)?;
        self.require_current(host, closing)?;
        if let Some((source, worker)) = &self.paired {
            if worker.holder_identity().is_some() {
                worker
                    .close_with_receipt(source, &self.proof)
                    .map_err(unavailable)?;
            } else {
                // Local transports can own an ordinary non-retained terminal.
                // A retained actor was required to provide a holder at capture.
                worker
                    .shutdown()
                    .map_err(|error| unavailable(error.to_string()))?;
            }
        }
        self.require_current(host, closing)?;
        retained_actor::finish_close(host, &self.target.handle, &self.target.source)
            .map_err(unavailable)?;
        self.require_current(host, closing)
    }

    /// Call only in the final commit lock, immediately before removing the chat.
    pub(super) fn remove_pair(
        &self,
        state: &mut HostState,
        closing: &capacity::Reservation,
    ) -> Result<Option<super::super::state::TerminalRecord>, ProtocolError> {
        self.require_locked(state, closing)?;
        Ok(self
            .paired
            .as_ref()
            .and_then(|(source, _)| state.retire_terminal(source.id)))
    }
}
