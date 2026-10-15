use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Condvar, Mutex},
};

use crate::{
    model::{HostSessionProof, TerminalId},
    terminal_ipc::{
        PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ServerEnvelope, ServerEnvelopeRef,
        ServerMessage, ServerResponse,
    },
};

use super::state::ClientId;

pub(crate) const DEFAULT_CLIENT_QUEUE_CAPACITY: usize = 512;
/// Native transcript pages can be several MiB. Two queued pages are sufficient
/// for the single-read-credit client; do not allow the ordinary small-message
/// count limit to retain gigabytes for a stalled peer.
const MAX_QUEUED_CHAT_PAGES: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryError {
    Closed,
    Full,
}

/// One queued frame, with the session proof shared rather than copied.
///
/// A redraw edge fans out to every visible subscriber of a terminal, and the
/// proof is three `String`s; cloning the owned [`ServerEnvelope`] per
/// subscriber made the per-frame allocation scale with the subscriber count.
/// The writer serializes this through [`ServerEnvelopeRef`], which encodes
/// byte-identically to the owned envelope.
#[derive(Clone, Debug)]
pub(crate) struct Outbound {
    version: u16,
    request_id: Option<u64>,
    proof: Arc<HostSessionProof>,
    pub(crate) message: ServerMessage,
}

impl Outbound {
    /// An unsolicited daemon-to-client message that shares `proof`.
    pub(crate) fn broadcast(proof: &Arc<HostSessionProof>, message: ServerMessage) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: None,
            proof: Arc::clone(proof),
            message,
        }
    }

    pub(crate) fn wire(&self) -> ServerEnvelopeRef<'_> {
        ServerEnvelopeRef {
            version: self.version,
            request_id: self.request_id,
            proof: &self.proof,
            message: &self.message,
        }
    }
}

impl From<ServerEnvelope> for Outbound {
    fn from(value: ServerEnvelope) -> Self {
        Self {
            version: value.version,
            request_id: value.request_id,
            proof: Arc::new(value.proof),
            message: value.message,
        }
    }
}

/// Message kinds that share one pending slot for a session.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SlotKind {
    Redraw,
    ChatChanged,
    ChatActivity,
    Bell,
}

fn coalescible(message: &ServerMessage) -> Option<(SlotKind, TerminalId)> {
    match message {
        ServerMessage::Redraw { terminal_id, .. } => Some((SlotKind::Redraw, *terminal_id)),
        ServerMessage::ChatChanged { chat_id } => Some((SlotKind::ChatChanged, *chat_id)),
        ServerMessage::ChatActivity { chat_id, .. } => Some((SlotKind::ChatActivity, *chat_id)),
        ServerMessage::Event {
            terminal_id,
            event: crate::terminal_ipc::WireTerminalEvent::Bell,
            ..
        } => Some((SlotKind::Bell, *terminal_id)),
        _ => None,
    }
}

/// Whether this message carries a whole native transcript page.
fn chat_page(message: &ServerMessage) -> bool {
    matches!(
        message,
        ServerMessage::Response {
            response: ServerResponse::ChatAttached { .. }
                | ServerResponse::ChatSnapshot { .. }
                | ServerResponse::ChatUpdate { .. }
                | ServerResponse::ChatListed { .. }
                | ServerResponse::ChatClosed { .. }
                | ServerResponse::ChatTerminal { .. }
                | ServerResponse::ChatFileChanges { .. }
                | ServerResponse::ChatFileChangePatches { .. }
                | ServerResponse::ChatProviderExitOutput { .. }
        }
    )
}

#[derive(Debug)]
struct QueueState {
    messages: VecDeque<Outbound>,
    /// Absolute queue position of the newest coalescible message of each kind,
    /// so `push` never scans the queue. A position is `popped` plus the index
    /// in `messages`; both are monotonic, so an entry can only be stale, never
    /// wrong, and a stale entry is discarded on lookup.
    slots: BTreeMap<(SlotKind, TerminalId), u64>,
    popped: u64,
    queued_chat_pages: usize,
    close_after_drain: bool,
    aborted: bool,
}

impl QueueState {
    /// The queue index of the newest coalescible message for `key`, if it is
    /// still queued.
    fn slot(&mut self, key: (SlotKind, TerminalId)) -> Option<usize> {
        let position = *self.slots.get(&key)?;
        let index = position
            .checked_sub(self.popped)
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| *index < self.messages.len());
        if index.is_none() {
            self.slots.remove(&key);
        }
        index
    }

    fn enqueue(&mut self, message: Outbound) {
        if let Some(key) = coalescible(&message.message) {
            let position = self.popped + self.messages.len() as u64;
            self.slots.insert(key, position);
        }
        if chat_page(&message.message) {
            self.queued_chat_pages += 1;
        }
        self.messages.push_back(message);
    }

    fn dequeue(&mut self) -> Option<Outbound> {
        let message = self.messages.pop_front()?;
        if let Some(key) = coalescible(&message.message)
            && self.slots.get(&key) == Some(&self.popped)
        {
            self.slots.remove(&key);
        }
        if chat_page(&message.message) {
            self.queued_chat_pages -= 1;
        }
        self.popped += 1;
        Some(message)
    }

    fn discard(&mut self) {
        self.popped += self.messages.len() as u64;
        self.messages.clear();
        self.slots.clear();
        self.queued_chat_pages = 0;
    }
}

#[derive(Debug)]
pub(crate) struct DeliveryQueue {
    client_id: ClientId,
    capacity: usize,
    state: Mutex<QueueState>,
    available: Condvar,
    async_available: tokio::sync::Notify,
}

impl DeliveryQueue {
    pub(crate) fn new(client_id: ClientId, capacity: usize) -> Self {
        Self {
            client_id,
            capacity: capacity.max(1),
            state: Mutex::new(QueueState {
                messages: VecDeque::new(),
                slots: BTreeMap::new(),
                popped: 0,
                queued_chat_pages: 0,
                close_after_drain: false,
                aborted: false,
            }),
            available: Condvar::new(),
            async_available: tokio::sync::Notify::new(),
        }
    }

    /// Enqueues a message without allowing a slow client to apply backpressure
    /// to a terminal worker. Redraw edges and chat activity for the same
    /// session are collapsed to the newest state through `slots`. Bells keep
    /// their earliest queued attention edge; other semantic events and
    /// responses remain ordered and lossless until the bounded queue
    /// disconnects the client.
    ///
    /// A disconnection is reported on the wire: the queue is replaced by a
    /// single retryable [`ProtocolErrorCode::QueueFull`] frame and closed after
    /// draining it, so the peer can tell "I fell behind" from "the daemon
    /// died" instead of observing a bare EOF.
    pub(crate) fn push(&self, message: impl Into<Outbound>) -> Result<(), DeliveryError> {
        let message = message.into();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.aborted || state.close_after_drain {
            return Err(DeliveryError::Closed);
        }

        // Coalesce through the indexed slot instead of scanning the queue.
        if let Some(key) = coalescible(&message.message)
            && let Some(index) = state.slot(key)
        {
            match &message.message {
                ServerMessage::Event {
                    event: crate::terminal_ipc::WireTerminalEvent::Bell,
                    ..
                } if state.messages[index].proof == message.proof => {
                    // Keep the earliest sequence and its queue position. A
                    // newer sequence here would overtake intervening exit or
                    // metadata events, which clients would then reject as old.
                    return Ok(());
                }
                ServerMessage::Redraw { generation, .. } => {
                    let generation = *generation;
                    if let ServerMessage::Redraw {
                        generation: queued, ..
                    } = &mut state.messages[index].message
                    {
                        *queued = (*queued).max(generation);
                        return Ok(());
                    }
                }
                ServerMessage::ChatChanged { .. } => {
                    if matches!(
                        state.messages[index].message,
                        ServerMessage::ChatChanged { .. }
                    ) {
                        return Ok(());
                    }
                }
                ServerMessage::ChatActivity { generation, .. } => {
                    let generation = *generation;
                    let existing = &state.messages[index];
                    let same_proof = existing.proof == message.proof;
                    let queued = match &existing.message {
                        ServerMessage::ChatActivity {
                            generation: queued, ..
                        } => Some(*queued),
                        _ => None,
                    };
                    if let Some(queued) = queued
                        && same_proof
                    {
                        if generation >= queued {
                            state.messages[index] = message;
                        }
                        return Ok(());
                    }
                }
                _ => {}
            }
        }

        let chat_backlog =
            chat_page(&message.message) && state.queued_chat_pages >= MAX_QUEUED_CHAT_PAGES;
        if state.messages.len() >= self.capacity || chat_backlog {
            let queued = state.messages.len();
            tracing::warn!(
                client = self.client_id,
                queued,
                capacity = self.capacity,
                chat_pages = state.queued_chat_pages,
                "disconnecting a cagents daemon client that fell behind its delivery queue"
            );
            state.discard();
            state.enqueue(Outbound {
                version: PROTOCOL_VERSION,
                request_id: None,
                proof: Arc::clone(&message.proof),
                message: ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::QueueFull,
                    format!(
                        "the cagents daemon dropped this connection: {queued} undelivered messages \
                         reached the per-client queue limit. Reconnect to resynchronize."
                    ),
                    true,
                )),
            });
            state.close_after_drain = true;
            self.available.notify_all();
            self.async_available.notify_one();
            return Err(DeliveryError::Full);
        }
        state.enqueue(message);
        self.available.notify_one();
        self.async_available.notify_one();
        Ok(())
    }

    pub(crate) fn pop(&self) -> Option<Outbound> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if state.aborted {
                return None;
            }
            if let Some(message) = state.dequeue() {
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

    pub(crate) async fn pop_async(&self) -> Option<Outbound> {
        loop {
            let available = self.async_available.notified();
            tokio::pin!(available);
            available.as_mut().enable();
            {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.aborted {
                    return None;
                }
                if let Some(message) = state.dequeue() {
                    return Some(message);
                }
                if state.close_after_drain {
                    return None;
                }
            }
            available.await;
        }
    }

    pub(crate) fn close_after_drain(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.close_after_drain = true;
        self.available.notify_all();
        self.async_available.notify_one();
    }

    /// Severs the connection without draining. `reason` is logged, because an
    /// abort discards queued frames and the peer only observes an EOF.
    pub(crate) fn abort(&self, reason: impl std::fmt::Display) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.aborted {
            tracing::warn!(
                client = self.client_id,
                queued = state.messages.len(),
                %reason,
                "aborting a cagents daemon client delivery queue"
            );
        }
        state.aborted = true;
        state.discard();
        self.available.notify_all();
        self.async_available.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        model::{BootId, HostIncarnation, HostSessionId},
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
        let queue = DeliveryQueue::new(1, 2);
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
    fn bell_storm_keeps_attention_and_completion_in_sequence() {
        let queue = DeliveryQueue::new(1, 2);
        let terminal_id = TerminalId::new();
        let mut envelope = redraw(terminal_id, 1);
        envelope.message = ServerMessage::Event {
            terminal_id,
            sequence: 1,
            event: crate::terminal_ipc::WireTerminalEvent::Bell,
        };
        queue.push(envelope.clone()).unwrap();
        envelope.message = ServerMessage::Event {
            terminal_id,
            sequence: 2,
            event: crate::terminal_ipc::WireTerminalEvent::Completed {
                exit_code: 0,
                signal: None,
                intentional: false,
            },
        };
        queue.push(envelope.clone()).unwrap();
        for sequence in 3..10_000 {
            envelope.message = ServerMessage::Event {
                terminal_id,
                sequence,
                event: crate::terminal_ipc::WireTerminalEvent::Bell,
            };
            queue.push(envelope.clone()).unwrap();
        }
        assert!(matches!(
            queue.pop().unwrap().message,
            ServerMessage::Event {
                sequence: 1,
                event: crate::terminal_ipc::WireTerminalEvent::Bell,
                ..
            }
        ));
        assert!(matches!(
            queue.pop().unwrap().message,
            ServerMessage::Event {
                sequence: 2,
                event: crate::terminal_ipc::WireTerminalEvent::Completed { .. },
                ..
            }
        ));
        // A new attention edge after the old slot drains must remain visible.
        queue.push(envelope).unwrap();
        assert!(matches!(
            queue.pop().unwrap().message,
            ServerMessage::Event {
                sequence: 9999,
                event: crate::terminal_ipc::WireTerminalEvent::Bell,
                ..
            }
        ));
    }

    /// Coalescing is indexed by absolute position, so a redraw that was already
    /// delivered must not be mistaken for a queued slot to overwrite.
    #[test]
    fn a_redraw_queued_after_its_predecessor_drained_is_delivered() {
        let queue = DeliveryQueue::new(1, 8);
        let first = TerminalId::new();
        let second = TerminalId::new();
        queue.push(redraw(first, 1)).unwrap();
        queue.push(redraw(second, 1)).unwrap();
        assert!(matches!(
            queue.pop().unwrap().message,
            ServerMessage::Redraw { generation: 1, .. }
        ));
        queue.push(redraw(first, 2)).unwrap();

        let mut generations = Vec::new();
        while let Some(message) = queue.pop() {
            let ServerMessage::Redraw {
                terminal_id,
                generation,
            } = message.message
            else {
                panic!("queue delivered an unexpected message kind");
            };
            generations.push((terminal_id, generation));
            if generations.len() == 2 {
                break;
            }
        }
        assert_eq!(generations, vec![(second, 1), (first, 2)]);
    }

    #[test]
    fn native_activity_coalesces_without_consuming_unread_or_regressing_generation() {
        use crate::agent::ActivityState;
        let queue = DeliveryQueue::new(1, 2);
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
                outcome: None,
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

    /// A client severed for backpressure must be able to report why, so the
    /// last frame it receives names the cause instead of being an EOF.
    #[test]
    fn overflow_reports_backpressure_on_the_wire_then_closes() {
        let queue = DeliveryQueue::new(7, 1);
        let mut envelope = redraw(TerminalId::new(), 1);
        envelope.request_id = Some(1);
        envelope.message = ServerMessage::Response {
            response: ServerResponse::Accepted,
        };
        queue.push(envelope.clone()).unwrap();
        assert_eq!(queue.push(envelope.clone()), Err(DeliveryError::Full));
        assert_eq!(queue.push(envelope), Err(DeliveryError::Closed));

        let delivered = queue.pop().unwrap();
        let ServerMessage::Error(error) = delivered.message else {
            panic!("a severed client was not told why");
        };
        assert_eq!(error.code, ProtocolErrorCode::QueueFull);
        assert!(error.retryable);
        assert!(queue.pop().is_none());
    }
}
