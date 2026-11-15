//! Nonblocking admission for language-server commands. Coalescing belongs
//! before initialization, not only in the task that eventually consumes them.
use super::*;

const MAX_COMMANDS: usize = 256;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_INCREMENTAL_BYTES: usize = 128 * 1024;
const MAX_INCREMENTAL_CHANGES: usize = 512;
const OVERFLOW: &str = "Language-server command backlog exceeded its bounded budget; restart the server to resynchronize";

pub(super) fn merge_incremental(
    previous: Option<Vec<lsp_types::TextDocumentContentChangeEvent>>,
    latest: Option<Vec<lsp_types::TextDocumentContentChangeEvent>>,
) -> Option<Vec<lsp_types::TextDocumentContentChangeEvent>> {
    match (previous, latest) {
        (Some(mut accumulated), Some(more))
            if accumulated.len() + more.len() <= MAX_INCREMENTAL_CHANGES
                && accumulated
                    .iter()
                    .chain(&more)
                    .map(|change| change.text.len())
                    .sum::<usize>()
                    <= MAX_INCREMENTAL_BYTES =>
        {
            accumulated.extend(more);
            Some(accumulated)
        }
        _ => None,
    }
}

#[derive(Default)]
struct Queue {
    commands: VecDeque<SessionCommand>,
    bytes: usize,
    closed: bool,
    overflow: bool,
    rejected_requests: Vec<PendingRequest>,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    ready: tokio::sync::Notify,
    stopped: tokio::sync::Notify,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn close(&self, discard: bool) {
        let mut queue = self.lock();
        queue.closed = true;
        if discard || queue.overflow {
            let commands = std::mem::take(&mut queue.commands);
            for command in commands {
                if queue.overflow
                    && let SessionCommand::Request(request) = command
                {
                    queue.rejected_requests.push(request);
                }
            }
            queue.bytes = 0;
        }
        drop(queue);
        self.ready.notify_one();
        self.stopped.notify_one();
    }
}

struct Senders(Arc<Shared>);

impl Drop for Senders {
    fn drop(&mut self) {
        // Drain admitted commands, including graceful shutdown, after the
        // final sender disappears. The receiver owns their bounded lifetime.
        self.0.close(false);
    }
}

#[derive(Clone)]
pub(super) struct Sender(Arc<Senders>);

pub(super) struct Receiver(Arc<Shared>);

pub(super) fn channel() -> (Sender, Receiver) {
    let shared = Arc::new(Shared::default());
    (
        Sender(Arc::new(Senders(Arc::clone(&shared)))),
        Receiver(shared),
    )
}

impl SessionCommand {
    fn queued_bytes(&self) -> usize {
        match self {
            Self::Open { text, .. } => text.byte_len(),
            Self::Save { text, .. } => text.len(),
            Self::Change {
                full, incremental, ..
            } => {
                full.byte_len()
                    + incremental.as_ref().map_or(0, |changes| {
                        changes
                            .iter()
                            .map(|change| change.text.len())
                            .sum::<usize>()
                    })
            }
            _ => 0,
        }
    }
}

impl Sender {
    pub(super) fn is_closed(&self) -> bool {
        self.0.0.lock().closed
    }

    pub(super) fn send(&self, mut command: SessionCommand) -> Result<(), &'static str> {
        if let SessionCommand::Change { incremental, .. } = &mut command {
            *incremental = merge_incremental(Some(Vec::new()), incremental.take());
        }
        let shared = &self.0.0;
        let mut queue = shared.lock();
        if queue.closed {
            return Err(if queue.overflow {
                OVERFLOW
            } else {
                "Language-server session closed"
            });
        }

        if matches!(command, SessionCommand::Shutdown) {
            queue.commands.clear();
            queue.bytes = 0;
            queue.commands.push_back(command);
            drop(queue);
            shared.ready.notify_one();
            return Ok(());
        }

        // Only the trailing change segment may coalesce. A save, request,
        // open or close is an ordering barrier: its text/version must not
        // silently become a later document state. Changes to other URIs do
        // not form a barrier, so alternating edits remain bounded too.
        let replacement = match &command {
            SessionCommand::Change { uri, .. } => queue
                .commands
                .iter()
                .enumerate()
                .rev()
                .take_while(|(_, queued)| matches!(queued, SessionCommand::Change { .. }))
                .find_map(|(index, queued)| match queued {
                    SessionCommand::Change { uri: previous, .. } if previous == uri => Some(index),
                    _ => None,
                }),
            _ => None,
        };
        let old_bytes = replacement.map_or(0, |index| queue.commands[index].queued_bytes());
        if let Some(index) = replacement
            && let SessionCommand::Change {
                incremental: previous,
                ..
            } = &mut queue.commands[index]
            && let SessionCommand::Change {
                incremental: latest,
                ..
            } = &mut command
        {
            // Compose ranges in version order, with their own bounded budget.
            // A missing/oversized sequence uses the latest full text instead.
            *latest = merge_incremental(previous.take(), latest.take());
        }
        let bytes = queue.bytes - old_bytes + command.queued_bytes();
        if bytes > MAX_BYTES || (replacement.is_none() && queue.commands.len() == MAX_COMMANDS) {
            // Never block input or silently discard an ordering barrier. Fail
            // the session, release its backlog immediately, and let the task
            // stop/reap the server and report an actionable failure.
            queue.overflow = true;
            drop(queue);
            shared.close(true);
            return Err(OVERFLOW);
        }
        if let Some(index) = replacement {
            queue.commands[index] = command;
        } else {
            queue.commands.push_back(command);
        }
        queue.bytes = bytes;
        drop(queue);
        shared.ready.notify_one();
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn pending_usage(&self) -> (usize, usize) {
        let queue = self.0.0.lock();
        (queue.commands.len(), queue.bytes)
    }
}

impl Receiver {
    pub(super) async fn recv(&mut self) -> Option<SessionCommand> {
        loop {
            let ready = self.0.ready.notified();
            {
                let mut queue = self.0.lock();
                if let Some(command) = queue.commands.pop_front() {
                    queue.bytes -= command.queued_bytes();
                    return Some(command);
                }
                if queue.closed {
                    return None;
                }
            }
            ready.await;
        }
    }

    pub(super) async fn closed(&self) {
        loop {
            let stopped = self.0.stopped.notified();
            if self.0.lock().closed {
                return;
            }
            stopped.await;
        }
    }

    pub(super) fn failure(&self) -> Option<&'static str> {
        self.0.lock().overflow.then_some(OVERFLOW)
    }

    pub(super) fn rejected_requests(&self) -> Vec<PendingRequest> {
        std::mem::take(&mut self.0.lock().rejected_requests)
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.0.close(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(uri: &Arc<Url>, version: i32, text: Arc<str>) -> SessionCommand {
        SessionCommand::Change {
            uri: Arc::clone(uri),
            version,
            incremental: Some(vec![lsp_types::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "x".into(),
            }]),
            full: Arc::new(Snapshot::new(version, text)),
        }
    }

    #[test]
    fn coalescing_releases_old_versions_and_keeps_uri_and_barrier_order() {
        let (sender, mut receiver) = channel();
        let one = Arc::new(Url::parse("file:///one.rs").unwrap());
        let two = Arc::new(Url::parse("file:///two.rs").unwrap());
        let previous: Arc<str> = Arc::from("previous");
        let weak = Arc::downgrade(&previous);
        sender.send(change(&one, 1, previous)).unwrap();
        sender.send(change(&two, 1, Arc::from("two"))).unwrap();
        for version in 2..1000 {
            sender
                .send(change(&one, version, Arc::from("latest")))
                .unwrap();
        }
        assert!(weak.upgrade().is_none());
        assert_eq!(
            sender.pending_usage(),
            (2, "latest".len() + "two".len() + 1)
        );
        sender
            .send(SessionCommand::Save {
                uri: Arc::clone(&one),
                text: Arc::from("latest"),
            })
            .unwrap();
        sender
            .send(change(&one, 1000, Arc::from("after save")))
            .unwrap();
        assert_eq!(sender.pending_usage().0, 4);
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            assert!(matches!(receiver.recv().await, Some(SessionCommand::Change { version: 999, incremental: None, .. })));
            assert!(matches!(receiver.recv().await, Some(SessionCommand::Change { version: 1, incremental: Some(_), .. })));
            assert!(matches!(receiver.recv().await, Some(SessionCommand::Save { text, .. }) if &*text == "latest"));
            assert!(matches!(receiver.recv().await, Some(SessionCommand::Change { version: 1000, incremental: Some(_), .. })));
            assert_eq!(sender.pending_usage(), (0, 0));
            drop(sender);
            assert!(receiver.recv().await.is_none());
        });
    }

    #[test]
    fn request_close_and_open_are_change_coalescing_barriers() {
        let uri = Arc::new(Url::parse("file:///one.rs").unwrap());
        for barrier in [
            SessionCommand::Request(PendingRequest {
                id: LspRequestId(1),
                doc: LspDocumentId(1),
                uri: Arc::clone(&uri),
                worktree: Arc::from(Path::new("/")),
                kind: RequestKind::Hover,
                position: lsp_types::Position::new(0, 0),
                version: 1,
            }),
            SessionCommand::Close {
                doc: LspDocumentId(1),
                uri: Arc::clone(&uri),
            },
            SessionCommand::Open {
                doc: LspDocumentId(1),
                uri: Arc::clone(&uri),
                language_id: "rust".into(),
                version: 1,
                text: Arc::new(Snapshot::new(1, Arc::from("opened"))),
            },
        ] {
            let (sender, _receiver) = channel();
            sender.send(change(&uri, 1, Arc::from("before"))).unwrap();
            sender.send(barrier).unwrap();
            sender.send(change(&uri, 2, Arc::from("after"))).unwrap();
            assert_eq!(sender.pending_usage().0, 3);
        }
    }

    #[test]
    fn first_change_also_bounds_incremental_payloads() {
        let (sender, receiver) = channel();
        let uri = Arc::new(Url::parse("file:///one.rs").unwrap());
        let mut command = change(&uri, 1, Arc::from("current text"));
        if let SessionCommand::Change { incremental, .. } = &mut command {
            incremental.as_mut().unwrap()[0].text = "x".repeat(MAX_INCREMENTAL_BYTES + 1);
        }
        sender.send(command).unwrap();
        assert_eq!(sender.pending_usage(), (1, "current text".len()));
        assert!(matches!(
            receiver.0.lock().commands.front(),
            Some(SessionCommand::Change {
                incremental: None,
                ..
            })
        ));
    }

    #[test]
    fn overload_closes_and_releases_payloads_instead_of_dropping_barriers() {
        let (sender, receiver) = channel();
        let uri = Arc::new(Url::parse("file:///one.rs").unwrap());
        let text: Arc<str> = Arc::from("x".repeat(MAX_BYTES / 2 + 1));
        let weak = Arc::downgrade(&text);
        sender
            .send(SessionCommand::Save {
                uri: Arc::clone(&uri),
                text: Arc::clone(&text),
            })
            .unwrap();
        assert!(sender.send(SessionCommand::Save { uri, text }).is_err());
        assert!(weak.upgrade().is_none());
        assert!(sender.is_closed());
        assert_eq!(sender.pending_usage(), (0, 0));
        assert_eq!(receiver.failure(), Some(OVERFLOW));

        let (sender, _receiver) = channel();
        for _ in 0..MAX_COMMANDS {
            sender
                .send(SessionCommand::Cancel {
                    request: LspRequestId(1),
                })
                .unwrap();
        }
        assert!(
            sender
                .send(SessionCommand::Cancel {
                    request: LspRequestId(2)
                })
                .is_err()
        );
        assert!(sender.is_closed());
    }

    #[test]
    fn final_sender_drop_preserves_graceful_shutdown() {
        let (sender, mut receiver) = channel();
        sender.send(SessionCommand::Shutdown).unwrap();
        drop(sender);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                assert!(matches!(
                    receiver.recv().await,
                    Some(SessionCommand::Shutdown)
                ));
                assert!(receiver.recv().await.is_none());
            });
    }

    #[test]
    fn shutdown_discards_backlog_and_receiver_drop_closes_admission() {
        let (sender, receiver) = channel();
        sender
            .send(SessionCommand::Cancel {
                request: LspRequestId(1),
            })
            .unwrap();
        sender.send(SessionCommand::Shutdown).unwrap();
        assert_eq!(sender.pending_usage(), (1, 0));
        drop(receiver);
        assert!(sender.is_closed());
        assert_eq!(sender.pending_usage(), (0, 0));
    }
}
