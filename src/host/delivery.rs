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
/// Native transcript, channel-work and Work board pages can be several MiB. They share
/// two queued page credits, preserving the existing single-read-credit bound
/// rather than letting the ordinary small-message count retain gigabytes.
const MAX_QUEUED_BULK_PAGES: usize = 2;

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

/// Replaceable state sharing one pending slot, by session or for the whole board.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SlotKey {
    Redraw(TerminalId),
    ChatChanged(TerminalId),
    ChatActivity(TerminalId),
    Bell(TerminalId),
    WorkBoard,
}

fn coalescible(message: &ServerMessage) -> Option<SlotKey> {
    match message {
        ServerMessage::Redraw { terminal_id, .. } => Some(SlotKey::Redraw(*terminal_id)),
        ServerMessage::ChatChanged { chat_id } => Some(SlotKey::ChatChanged(*chat_id)),
        ServerMessage::ChatActivity { chat_id, .. } => Some(SlotKey::ChatActivity(*chat_id)),
        ServerMessage::Event {
            terminal_id,
            event: crate::terminal_ipc::WireTerminalEvent::Bell,
            ..
        } => Some(SlotKey::Bell(*terminal_id)),
        ServerMessage::WorkBoard { .. } => Some(SlotKey::WorkBoard),
        _ => None,
    }
}

/// Whether this response consumes the existing shared bulk-page credit.
fn bulk_page(message: &ServerMessage) -> bool {
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
                | ServerResponse::ChatTextCopy { .. }
                | ServerResponse::Agent(crate::agent_execution::Reply::ChatPage(_))
                | ServerResponse::ChannelWork(
                    crate::channel_work_api::Response::Snapshot(_)
                        | crate::channel_work_api::Response::Applied(_)
                        | crate::channel_work_api::Response::Target(_)
                )
                | ServerResponse::Work(
                    crate::work::Response::Board(_) | crate::work::Response::Watching { .. }
                )
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
    slots: BTreeMap<SlotKey, u64>,
    popped: u64,
    queued_bulk_pages: usize,
    close_after_drain: bool,
    aborted: bool,
}

impl QueueState {
    /// The queue index of the newest coalescible message for `key`, if it is
    /// still queued.
    fn slot(&mut self, key: SlotKey) -> Option<usize> {
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
        if bulk_page(&message.message) {
            self.queued_bulk_pages += 1;
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
        if bulk_page(&message.message) {
            self.queued_bulk_pages -= 1;
        }
        self.popped += 1;
        Some(message)
    }

    fn discard(&mut self) {
        self.popped += self.messages.len() as u64;
        self.messages.clear();
        self.slots.clear();
        self.queued_bulk_pages = 0;
    }
}

#[derive(Debug)]
pub(crate) struct DeliveryQueue {
    client_id: ClientId,
    capacity: usize,
    state: Mutex<QueueState>,
    available: Condvar,
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
                queued_bulk_pages: 0,
                close_after_drain: false,
                aborted: false,
            }),
            available: Condvar::new(),
        }
    }

    /// Enqueues a message without allowing a slow client to apply backpressure
    /// to a terminal worker. Redraw edges and chat activity for the same
    /// session and full Work board publications are collapsed to the newest
    /// state through `slots`. Bells keep
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
                ServerMessage::WorkBoard { revision, .. } => {
                    let existing = &state.messages[index];
                    if existing.proof == message.proof
                        && let ServerMessage::WorkBoard {
                            revision: queued, ..
                        } = &existing.message
                    {
                        // These are complete snapshots. A later board may
                        // replace an earlier publication in place, but never
                        // a correlated reply or another daemon's proof.
                        if revision >= queued {
                            state.messages[index] = message;
                        }
                        return Ok(());
                    }
                }
                _ => {}
            }
        }

        let bulk_backlog =
            bulk_page(&message.message) && state.queued_bulk_pages >= MAX_QUEUED_BULK_PAGES;
        if state.messages.len() >= self.capacity || bulk_backlog {
            let queued = state.messages.len();
            tracing::warn!(
                client = self.client_id,
                queued,
                capacity = self.capacity,
                bulk_pages = state.queued_bulk_pages,
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
            return Err(DeliveryError::Full);
        }
        state.enqueue(message);
        self.available.notify_one();
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

    pub(crate) fn close_after_drain(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.close_after_drain = true;
        self.available.notify_all();
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

    fn board(marker: u64) -> crate::work::Board {
        crate::work::Board {
            generated_at_unix_ms: marker,
            stale_after_days: 7,
            projects: Vec::new(),
            items: Vec::new(),
            linear_unavailable: Some("x".repeat(64 * 1024)),
        }
    }

    fn work_board(revision: u64) -> ServerEnvelope {
        let mut envelope = redraw(TerminalId::new(), 0);
        envelope.message = ServerMessage::WorkBoard {
            revision,
            board: Box::new(board(revision)),
        };
        envelope
    }

    #[test]
    fn peer_pages_share_bulk_credit_keep_request_order_and_release_on_overflow_or_abort() {
        let peer = |request_id| {
            let mut page = crate::agent_execution::inspection::Page::new(
                TerminalId::new(),
                crate::agent_execution::inspection::Coverage::Cached,
            );
            page.at_end = true;
            let mut envelope = redraw(page.session, 0);
            envelope.request_id = Some(request_id);
            envelope.message = ServerMessage::Response {
                response: ServerResponse::Agent(crate::agent_execution::Reply::ChatPage(page)),
            };
            envelope
        };
        let queue = DeliveryQueue::new(1, 16);
        queue.push(peer(1)).unwrap();
        let mut work = work_board(1);
        work.request_id = Some(2);
        work.message = ServerMessage::Response {
            response: ServerResponse::Work(crate::work::Response::Board(board(1))),
        };
        queue.push(work).unwrap();
        assert_eq!(queue.state.lock().unwrap().queued_bulk_pages, 2);
        assert_eq!(queue.pop().unwrap().request_id, Some(1));
        queue.push(peer(3)).unwrap();
        assert_eq!(queue.state.lock().unwrap().queued_bulk_pages, 2);
        assert_eq!(queue.pop().unwrap().request_id, Some(2));
        assert_eq!(queue.pop().unwrap().request_id, Some(3));
        assert_eq!(queue.state.lock().unwrap().queued_bulk_pages, 0);

        queue.push(peer(4)).unwrap();
        queue.push(peer(5)).unwrap();
        assert_eq!(queue.push(peer(6)), Err(DeliveryError::Full));
        assert_eq!(queue.state.lock().unwrap().queued_bulk_pages, 0);
        assert!(matches!(
            queue.pop().unwrap().message,
            ServerMessage::Error(ProtocolError {
                code: ProtocolErrorCode::QueueFull,
                retryable: true,
                ..
            })
        ));
        assert!(queue.pop().is_none());

        let aborted = DeliveryQueue::new(2, 16);
        aborted.push(peer(7)).unwrap();
        aborted.abort("test client disconnected");
        assert_eq!(aborted.state.lock().unwrap().queued_bulk_pages, 0);
        assert!(aborted.pop().is_none());
    }

    #[test]
    fn work_board_bursts_keep_one_latest_payload_and_preserve_semantic_replies() {
        let queue = DeliveryQueue::new(1, 3);
        let accepted = |request_id| {
            let mut envelope = redraw(TerminalId::new(), 0);
            envelope.request_id = Some(request_id);
            envelope.message = ServerMessage::Response {
                response: ServerResponse::Accepted,
            };
            envelope
        };
        queue.push(accepted(1)).unwrap();
        queue.push(work_board(1)).unwrap();
        queue.push(accepted(2)).unwrap();
        // A queue at its count limit must still accept replacement snapshots.
        for revision in 2..=128 {
            queue.push(work_board(revision)).unwrap();
        }
        queue.push(work_board(4)).unwrap();
        {
            let state = queue.state.lock().unwrap();
            assert_eq!(state.messages.len(), 3);
            assert_eq!(state.slots.len(), 1);
            assert_eq!(state.queued_bulk_pages, 0);
        }
        assert_eq!(queue.pop().unwrap().request_id, Some(1));
        let published = queue.pop().unwrap();
        assert_eq!(published.request_id, None);
        assert!(matches!(published.message,
            ServerMessage::WorkBoard { revision: 128, board }
                if board.generated_at_unix_ms == 128
                    && board.linear_unavailable.as_ref().unwrap().len() == 64 * 1024
        ));
        assert_eq!(queue.pop().unwrap().request_id, Some(2));
        // A drained slot cannot swallow the next publication.
        queue.push(work_board(129)).unwrap();
        assert!(matches!(
            queue.pop().unwrap().message,
            ServerMessage::WorkBoard { revision: 129, .. }
        ));
    }

    #[test]
    fn work_board_publications_do_not_replace_another_daemon_proof() {
        let queue = DeliveryQueue::new(1, 2);
        queue.push(work_board(5)).unwrap();
        let mut replacement = work_board(1);
        replacement.proof.host_incarnation = HostIncarnation::new("replacement");
        queue.push(replacement.clone()).unwrap();
        assert!(matches!(
            queue.pop().unwrap().message,
            ServerMessage::WorkBoard { revision: 5, .. }
        ));
        assert_eq!(queue.pop().unwrap().wire().proof, &replacement.proof);
    }

    #[test]
    fn work_board_replies_share_bulk_credit_without_coalescing_correlated_responses() {
        let reply = |request_id, response| {
            let mut envelope = work_board(99);
            envelope.request_id = Some(request_id);
            envelope.message = ServerMessage::Response { response };
            envelope
        };
        let pages = [
            ServerResponse::Work(crate::work::Response::Board(board(1))),
            ServerResponse::Work(crate::work::Response::Watching {
                revision: 2,
                board: Box::new(board(2)),
            }),
        ];
        for page in pages {
            let queue = DeliveryQueue::new(1, DEFAULT_CLIENT_QUEUE_CAPACITY);
            queue.push(reply(1, page.clone())).unwrap();
            queue.push(reply(2, page.clone())).unwrap();
            queue.push(work_board(3)).unwrap();
            queue.push(work_board(4)).unwrap();
            assert_eq!(queue.state.lock().unwrap().queued_bulk_pages, 2);
            let first = queue.pop().unwrap();
            assert_eq!(first.request_id, Some(1));
            assert!(matches!(first.message,
                ServerMessage::Response { response } if response == page
            ));
            assert_eq!(queue.state.lock().unwrap().queued_bulk_pages, 1);
            queue
                .push(reply(3, ServerResponse::ChatListed { sessions: vec![] }))
                .unwrap();
            assert_eq!(queue.pop().unwrap().request_id, Some(2));
            assert_eq!(queue.state.lock().unwrap().queued_bulk_pages, 1);
            assert!(matches!(
                queue.pop().unwrap().message,
                ServerMessage::WorkBoard { revision: 4, .. }
            ));
            queue.push(reply(4, page.clone())).unwrap();
            assert_eq!(queue.push(reply(5, page.clone())), Err(DeliveryError::Full));
            {
                let state = queue.state.lock().unwrap();
                assert_eq!(state.queued_bulk_pages, 0);
                assert!(state.slots.is_empty());
            }
            let ServerMessage::Error(error) = queue.pop().unwrap().message else {
                panic!("bulk overflow must explain why the connection closed")
            };
            assert_eq!(error.code, ProtocolErrorCode::QueueFull);
            assert!(error.retryable);
            assert_eq!(queue.push(reply(6, page)), Err(DeliveryError::Closed));
            assert!(queue.pop().is_none());
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
    fn text_copy_pages_share_bulk_backpressure_and_release_credit_when_drained() {
        let queue = DeliveryQueue::new(7, DEFAULT_CLIENT_QUEUE_CAPACITY);
        let chat_id = TerminalId::new();
        let source = crate::chat::TextCopySource {
            cache: uuid::Uuid::new_v4(),
            scope: crate::chat::TextCopyScope::Conversation,
            token: 1,
            revision: 1,
            bytes: (4 * crate::chat::TEXT_COPY_PAGE_BYTES) as u64,
        };
        let page = |request| {
            let mut envelope = redraw(chat_id, 0);
            envelope.request_id = Some(request);
            envelope.message = ServerMessage::Response {
                response: ServerResponse::ChatTextCopy {
                    chat_id,
                    page: crate::chat::TextCopyPage {
                        source: source.clone(),
                        offset: (request - 1) * crate::chat::TEXT_COPY_PAGE_BYTES as u64,
                        text: "x".repeat(crate::chat::TEXT_COPY_PAGE_BYTES),
                        done: request == 4,
                    },
                },
            };
            envelope
        };
        queue.push(page(1)).unwrap();
        queue.push(page(2)).unwrap();
        let first = queue.pop().unwrap();
        assert_eq!(first.request_id, Some(1));
        assert!(matches!(first.message, ServerMessage::Response {
            response: ServerResponse::ChatTextCopy { page, .. }
        } if page.offset == 0 && page.text.len() == crate::chat::TEXT_COPY_PAGE_BYTES));
        // Draining one page admits the next; a third retained page must still
        // fail at the bulk limit, well before the ordinary 512-frame capacity.
        queue.push(page(3)).unwrap();
        assert_eq!(queue.push(page(4)), Err(DeliveryError::Full));
        assert_eq!(queue.push(page(4)), Err(DeliveryError::Closed));
        let delivered = queue.pop().unwrap();
        let ServerMessage::Error(error) = delivered.message else {
            panic!("a stalled text-copy peer was not told why");
        };
        assert_eq!(error.code, ProtocolErrorCode::QueueFull);
        assert!(error.retryable);
        assert!(queue.pop().is_none());
    }

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

    #[test]
    fn channel_work_pages_share_chat_credit_preserve_order_and_close_on_mixed_overflow() {
        use crate::channel_work::{
            Home, HomeLifecycle, Receipt, Reply, Snapshot, Target, TargetSnapshot,
        };
        use crate::channel_work_api::Response;
        use crate::github_identity::{GitHubRepoKey, PullRequestKey};
        let home = Home {
            registry: uuid::Uuid::new_v4(),
            channel: crate::model::ChannelId::new(),
            creation_ordinal: 1,
        };
        let snapshot = Snapshot {
            home,
            lifecycle: HomeLifecycle::Active,
            revision: 0,
            entries: vec![],
            history: vec![],
            next_current: None,
            next_history: None,
        };
        let target = Target::PullRequest(
            PullRequestKey::new(
                GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
                1,
            )
            .unwrap(),
        );
        let pages = [
            Response::Snapshot(snapshot.clone()),
            Response::Applied(Reply {
                receipt: Receipt {
                    operation: uuid::Uuid::new_v4(),
                    home,
                    revision: 0,
                    sequence: 0,
                    changed: false,
                },
                snapshot,
            }),
            Response::Target(TargetSnapshot {
                home,
                lifecycle: HomeLifecycle::Active,
                revision: 0,
                target,
                entry: None,
            }),
        ];
        let envelope = |id, response| {
            let mut envelope = redraw(TerminalId::new(), 0);
            envelope.request_id = Some(id);
            envelope.message = ServerMessage::Response { response };
            envelope
        };
        for page in pages {
            let queue = DeliveryQueue::new(7, DEFAULT_CLIENT_QUEUE_CAPACITY);
            queue
                .push(envelope(1, ServerResponse::ChatListed { sessions: vec![] }))
                .unwrap();
            queue
                .push(envelope(2, ServerResponse::ChannelWork(page.clone())))
                .unwrap();
            assert_eq!(queue.state.lock().unwrap().queued_bulk_pages, 2);
            assert_eq!(queue.pop().unwrap().request_id, Some(1));
            assert_eq!(queue.state.lock().unwrap().queued_bulk_pages, 1);
            queue
                .push(envelope(3, ServerResponse::ChannelWork(page.clone())))
                .unwrap();
            let first_work = queue.pop().unwrap();
            assert_eq!(first_work.request_id, Some(2));
            assert!(
                matches!(first_work.message, ServerMessage::Response { response: ServerResponse::ChannelWork(returned) } if returned == page)
            );
            queue
                .push(envelope(4, ServerResponse::ChatListed { sessions: vec![] }))
                .unwrap();
            assert_eq!(
                queue.push(envelope(5, ServerResponse::ChannelWork(page.clone()))),
                Err(DeliveryError::Full)
            );
            assert_eq!(
                queue.state.lock().unwrap().queued_bulk_pages,
                0,
                "overflow discards every bulk credit"
            );
            assert_eq!(
                queue.push(envelope(6, ServerResponse::ChannelWork(page))),
                Err(DeliveryError::Closed)
            );
            let ServerMessage::Error(error) = queue.pop().unwrap().message else {
                panic!("bounded queue must report its overflow")
            };
            assert_eq!(error.code, ProtocolErrorCode::QueueFull);
            assert!(error.retryable);
            assert!(queue.pop().is_none());
        }
    }
}
