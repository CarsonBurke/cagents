use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex},
};

use crate::terminal_ipc::{ServerEnvelope, ServerMessage};

pub(crate) const DEFAULT_CLIENT_QUEUE_CAPACITY: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryError {
    Closed,
    Full,
}

#[derive(Debug)]
struct QueueState {
    messages: VecDeque<ServerEnvelope>,
    close_after_drain: bool,
    aborted: bool,
}

#[derive(Debug)]
pub(crate) struct DeliveryQueue {
    capacity: usize,
    state: Mutex<QueueState>,
    available: Condvar,
}

impl DeliveryQueue {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            state: Mutex::new(QueueState {
                messages: VecDeque::new(),
                close_after_drain: false,
                aborted: false,
            }),
            available: Condvar::new(),
        }
    }

    /// Enqueues a message without allowing a slow client to apply backpressure
    /// to a terminal worker. Redraw edges for the same terminal are collapsed
    /// to the newest generation while responses and semantic events remain
    /// ordered and lossless until the bounded queue disconnects the client.
    pub(crate) fn push(&self, message: ServerEnvelope) -> Result<(), DeliveryError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.aborted || state.close_after_drain {
            return Err(DeliveryError::Closed);
        }

        if let ServerMessage::Redraw {
            terminal_id,
            generation,
        } = &message.message
            && let Some(existing) = state.messages.iter_mut().find(|candidate| {
                matches!(
                    &candidate.message,
                    ServerMessage::Redraw {
                        terminal_id: existing_id,
                        ..
                    } if existing_id == terminal_id
                )
            })
        {
            if let ServerMessage::Redraw {
                generation: existing_generation,
                ..
            } = &mut existing.message
            {
                *existing_generation = (*existing_generation).max(*generation);
            }
            return Ok(());
        }
        if let ServerMessage::ChatChanged { chat_id } = &message.message
            && state.messages.iter().any(|candidate| {
                matches!(&candidate.message,
                ServerMessage::ChatChanged { chat_id: existing } if existing == chat_id)
            })
        {
            return Ok(());
        }
        if let ServerMessage::ChatActivity {
            chat_id,
            generation,
            ..
        } = &message.message
            && let Some(existing) = state.messages.iter_mut().find(|candidate| {
                candidate.proof == message.proof
                    && matches!(&candidate.message,
                    ServerMessage::ChatActivity { chat_id: existing, .. } if existing == chat_id)
            })
        {
            if let ServerMessage::ChatActivity {
                generation: previous,
                ..
            } = &existing.message
                && generation >= previous
            {
                *existing = message;
            }
            return Ok(());
        }

        // Native transcript pages can be several MiB. Two queued pages are
        // sufficient for the single-read-credit client; do not allow the ordinary
        // small-message count limit to retain gigabytes for a stalled peer.
        let large_chat = |message: &ServerMessage| {
            matches!(
                message,
                ServerMessage::Response {
                    response: crate::terminal_ipc::ServerResponse::ChatAttached { .. }
                        | crate::terminal_ipc::ServerResponse::ChatSnapshot { .. }
                        | crate::terminal_ipc::ServerResponse::ChatListed { .. }
                        | crate::terminal_ipc::ServerResponse::ChatClosed { .. }
                }
            )
        };
        let chat_backlog = large_chat(&message.message)
            && state
                .messages
                .iter()
                .filter(|queued| large_chat(&queued.message))
                .take(2)
                .count()
                == 2;
        if state.messages.len() >= self.capacity || chat_backlog {
            state.aborted = true;
            state.messages.clear();
            self.available.notify_all();
            return Err(DeliveryError::Full);
        }
        state.messages.push_back(message);
        self.available.notify_one();
        Ok(())
    }

    pub(crate) fn pop(&self) -> Option<ServerEnvelope> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if state.aborted {
                return None;
            }
            if let Some(message) = state.messages.pop_front() {
                return Some(message);
            }
            if state.close_after_drain {
                return None;
            }
            state = self
                .available
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub(crate) fn close_after_drain(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.close_after_drain = true;
        self.available.notify_all();
    }

    pub(crate) fn abort(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.aborted = true;
        state.messages.clear();
        self.available.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        model::{BootId, HostIncarnation, HostSessionId, TerminalId},
        terminal_ipc::{PROTOCOL_VERSION, ServerMessage},
    };

    use super::*;

    fn redraw(terminal_id: TerminalId, generation: u64) -> ServerEnvelope {
        ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: None,
            proof: crate::model::HostSessionProof {
                host_session_id: HostSessionId::new("session"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            },
            message: ServerMessage::Redraw {
                terminal_id,
                generation,
            },
        }
    }

    #[test]
    fn stale_redraw_generation_cannot_replace_a_newer_one() {
        let queue = DeliveryQueue::new(2);
        let terminal_id = TerminalId::new();
        queue.push(redraw(terminal_id, 8)).unwrap();
        queue.push(redraw(terminal_id, 3)).unwrap();
        queue.push(redraw(terminal_id, 11)).unwrap();

        let delivered = queue.pop().unwrap();
        assert!(matches!(
            delivered.message,
            ServerMessage::Redraw { generation: 11, .. }
        ));
    }

    #[test]
    fn native_activity_coalesces_without_consuming_unread_or_regressing_generation() {
        use crate::agent::ActivityState;
        let queue = DeliveryQueue::new(2);
        let chat_id = TerminalId::new();
        let mut envelope = redraw(chat_id, 0);
        envelope.message = ServerMessage::ChatObserved { chat_id, epoch: 0 };
        queue.push(envelope.clone()).unwrap();
        for (generation, state) in [
            (3, ActivityState::Working),
            (5, ActivityState::Waiting),
            (4, ActivityState::Idle),
        ] {
            envelope.message = ServerMessage::ChatActivity {
                chat_id,
                state,
                generation,
            };
            queue.push(envelope.clone()).unwrap();
        }
        assert!(matches!(
            queue.pop().unwrap().message,
            ServerMessage::ChatObserved { .. }
        ));
        assert!(matches!(
            queue.pop().unwrap().message,
            ServerMessage::ChatActivity {
                state: ActivityState::Waiting,
                generation: 5,
                ..
            }
        ));
    }
}
