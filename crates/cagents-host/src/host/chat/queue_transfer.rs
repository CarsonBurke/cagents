use super::{ClientId, error, retryable_unavailable};
use crate::{
    chat::ExitedQueue,
    model::TerminalId,
    terminal_ipc::{
        EXITED_QUEUE_PAGE_BYTES, ExitedQueuePage, MAX_EXITED_QUEUE_BYTES, ProtocolError,
    },
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use uuid::Uuid;

/// Bound the bytes retained across connections independently of any provider's
/// per-queue limit. Completed, disconnected and superseded transfers release
/// their buffers. Later reads or starts also reclaim timed-out transfers.
const MAX_PENDING_BYTES: usize = 108 * 1024 * 1024;
const PAGE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Default)]
pub(in crate::host) struct Transfers {
    active: BTreeMap<ClientId, Transfer>,
}

struct Transfer {
    source: TerminalId,
    receipt: Uuid,
    bytes: Vec<u8>,
    next: usize,
    expires: Instant,
}

impl Transfer {
    fn page(&mut self, now: Instant) -> ExitedQueuePage {
        let offset = self.next;
        self.next = (offset + EXITED_QUEUE_PAGE_BYTES).min(self.bytes.len());
        self.expires = now + PAGE_TIMEOUT;
        ExitedQueuePage {
            receipt: self.receipt,
            total: self.bytes.len() as u32,
            offset: offset as u32,
            bytes: self.bytes[offset..self.next].to_vec(),
        }
    }
}

pub(super) fn encode(queue: &ExitedQueue) -> Result<Vec<u8>, ProtocolError> {
    let size = postcard::experimental::serialized_size(queue)
        .map_err(|_| error("Could not encode the retired input queue"))?;
    if size > MAX_EXITED_QUEUE_BYTES {
        return Err(error("The retired input queue exceeds its replay limit"));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| retryable_unavailable("Could not allocate the retired input queue"))?;
    bytes.resize(size, 0);
    postcard::to_slice(queue, &mut bytes)
        .map_err(|_| error("Could not encode the retired input queue"))?;
    Ok(bytes)
}

impl Transfers {
    pub(in crate::host) fn remove(&mut self, client: ClientId) {
        self.active.remove(&client);
    }

    pub(super) fn begin(
        &mut self,
        client: ClientId,
        source: TerminalId,
        bytes: Vec<u8>,
        now: Instant,
    ) -> Result<ExitedQueuePage, ProtocolError> {
        self.remove(client);
        self.active.retain(|_, transfer| transfer.expires > now);
        let mut transfer = Transfer {
            source,
            receipt: Uuid::new_v4(),
            bytes,
            next: 0,
            expires: now,
        };
        if transfer.bytes.len() > EXITED_QUEUE_PAGE_BYTES
            && self
                .active
                .values()
                .map(|other| other.bytes.len())
                .sum::<usize>()
                + transfer.bytes.len()
                > MAX_PENDING_BYTES
        {
            return Err(retryable_unavailable(
                "Other retired input queues are still transferring. Retry shortly",
            ));
        }
        let page = transfer.page(now);
        if transfer.next < transfer.bytes.len() {
            self.active.insert(client, transfer);
        }
        Ok(page)
    }

    pub(super) fn read(
        &mut self,
        client: ClientId,
        source: TerminalId,
        receipt: Uuid,
        offset: u32,
        now: Instant,
    ) -> Result<ExitedQueuePage, ProtocolError> {
        self.active.retain(|_, transfer| transfer.expires > now);
        let transfer = self.active.get_mut(&client).ok_or_else(|| {
            retryable_unavailable("The retired input queue transfer ended. Retry Resume")
        })?;
        if transfer.source != source
            || transfer.receipt != receipt
            || transfer.next != offset as usize
        {
            return Err(error(
                "The retired input queue page does not match its transfer",
            ));
        }
        let page = transfer.page(now);
        if transfer.next == transfer.bytes.len() {
            self.remove(client);
        }
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_belong_to_one_connection_source_receipt_and_offset() {
        let mut transfers = Transfers::default();
        let now = Instant::now();
        let source = TerminalId::new();
        let bytes = vec![1; EXITED_QUEUE_PAGE_BYTES + 1];
        let first = transfers.begin(1, source, bytes, now).unwrap();
        assert_eq!(first.offset, 0);
        assert_eq!(first.bytes.len(), EXITED_QUEUE_PAGE_BYTES);
        let next = EXITED_QUEUE_PAGE_BYTES as u32;
        for (client, source, receipt, offset) in [
            (2, source, first.receipt, next),
            (1, TerminalId::new(), first.receipt, next),
            (1, source, Uuid::new_v4(), next),
            (1, source, first.receipt, 0),
        ] {
            assert!(
                transfers
                    .read(client, source, receipt, offset, now)
                    .is_err()
            );
        }
        let last = transfers.read(1, source, first.receipt, next, now).unwrap();
        assert_eq!(last.bytes, [1]);
        assert_eq!(last.total, first.total);
        assert!(transfers.read(1, source, first.receipt, next, now).is_err());
    }

    #[test]
    fn the_daemon_rejects_a_queue_page_from_another_daemons_proof() {
        use crate::{
            host::{
                client,
                state::{HostLock, HostState},
            },
            model::HostSessionProof,
            terminal_ipc::{
                ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolErrorCode, ServerEnvelope,
                ServerMessage, ServerResponse, read_frame, write_frame,
            },
        };
        use std::{os::unix::net::UnixStream, sync::Arc, thread};

        let client_id = 13;
        let source = TerminalId::new();
        let proof = HostSessionProof {
            host_session_id: "queue-transfer".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "current-incarnation".into(),
        };
        let mut host = HostState::new(proof.clone());
        let first = host
            .exited_queue_transfers
            .begin(
                client_id,
                source,
                vec![1; EXITED_QUEUE_PAGE_BYTES * 2 + 1],
                Instant::now(),
            )
            .unwrap();
        let host = Arc::new(HostLock::new(host));
        let (mut request_stream, server_stream) = UnixStream::pair().unwrap();
        request_stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let worker = thread::spawn(move || client::serve(server_stream, client_id, host));
        let mut request = |request_id, proof, request| {
            write_frame(
                &mut request_stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id,
                    proof,
                    request,
                },
            )
            .unwrap();
            let reply: ServerEnvelope = read_frame(&mut request_stream).unwrap().unwrap();
            assert_eq!(reply.request_id, Some(request_id));
            reply.message
        };
        assert!(matches!(
            request(
                1,
                None,
                ClientRequest::Connect {
                    expected_host: None,
                    resume: Vec::new(),
                }
            ),
            ServerMessage::Response {
                response: ServerResponse::Connected { .. }
            }
        ));
        let read = |offset| ClientRequest::ChatReadExitedQueuePage {
            source,
            receipt: first.receipt,
            offset: offset as u32,
        };
        assert!(matches!(
            request(
                2,
                Some(proof.clone()),
                read(EXITED_QUEUE_PAGE_BYTES)
            ),
            ServerMessage::Response { response: ServerResponse::ChatExitedQueuePage { source: actual, page } }
                if actual == source && page.offset == EXITED_QUEUE_PAGE_BYTES as u32
        ));
        assert!(matches!(
            request(
                3,
                Some(HostSessionProof {
                    host_incarnation: "different-incarnation".into(),
                    ..proof
                }),
                read(EXITED_QUEUE_PAGE_BYTES * 2)
            ),
            ServerMessage::Error(ProtocolError {
                code: ProtocolErrorCode::ProofMismatch,
                ..
            })
        ));
        drop(request_stream);
        worker.join().unwrap();
    }

    #[test]
    fn reconnect_recovers_the_prior_exact_queue_while_its_replacement_runs() {
        use crate::{
            chat::{
                QueuedMessage,
                client::{ClientHandle, Wake},
            },
            control::ControlClient,
            host::{
                chat::tests::{
                    completion_wake_host, completion_wake_record, direct_completion_actor,
                },
                client,
            },
            model::ChatDraftModel,
            terminal_ipc::{
                ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
                ServerResponse, read_frame, write_frame,
            },
        };
        use std::{os::unix::net::UnixStream, sync::Arc, thread};

        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let host = completion_wake_host();
        let id = TerminalId::new();
        let replacement = Uuid::new_v4();
        let (handle, launch) = direct_completion_actor("exec /bin/cat", root.path(), |_| {});
        let original = completion_wake_record(&host, id, replacement, handle.clone(), launch);
        let (source, archive) = {
            let state = host.lock().unwrap();
            (
                state.chats[&id].current_session(),
                Arc::clone(&state.exited_queue_archive),
            )
        };
        let execution = Uuid::new_v4();
        let message = QueuedMessage {
            id: "prior-input".into(),
            text: "The previous provider died while this was held.".into(),
            editable: true,
            delivery: Default::default(),
            attachments: vec![],
        };
        let queue = ExitedQueue {
            messages: vec![message.clone()],
            account_scope: None,
            confirmed_session: source.launch.session_id.clone(),
        };
        archive
            .retain_in(
                root.path(),
                original.clone(),
                source.clone(),
                execution,
                crate::chat::exited_queue_fingerprint([message.id.as_str()]),
                &encode(&queue).unwrap(),
            )
            .unwrap();
        let connect = |id| {
            let (window, daemon) = UnixStream::pair().unwrap();
            let host = Arc::clone(&host);
            (
                window,
                thread::spawn(move || client::serve(daemon, id, host)),
            )
        };
        // Lose a window after it read the bytes but before its save/ACK.
        let (window, worker) = connect(91);
        let mut lost = ControlClient::from_stream(window).unwrap();
        assert_eq!(
            lost.read_exited_chat_queue(original).unwrap(),
            Some(queue.clone())
        );
        drop(lost);
        worker.join().unwrap();

        // Normal attachment ignores a stale draft preference. It may still
        // return the hosted descriptor but must not announce a prior receipt.
        let mut altered = source.clone();
        altered.launch.draft_model = Some(Box::new(ChatDraftModel {
            session: id,
            provider: source.launch.provider,
            model: Some("stale-draft-choice".into()),
            reasoning: None,
        }));
        let (mut window, worker) = connect(94);
        window
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        for (request_id, proof, request) in [
            (
                1,
                None,
                ClientRequest::Connect {
                    expected_host: None,
                    resume: Vec::new(),
                },
            ),
            (
                2,
                source.proof.clone(),
                ClientRequest::ChatAttach { session: altered },
            ),
        ] {
            write_frame(
                &mut window,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id,
                    proof,
                    request,
                },
            )
            .unwrap();
            let reply: ServerEnvelope = read_frame(&mut window).unwrap().unwrap();
            assert_eq!(reply.request_id, Some(request_id));
            assert!(matches!(
                reply.message,
                ServerMessage::Response {
                    response: ServerResponse::Connected { .. }
                        | ServerResponse::ChatAttached { .. }
                }
            ));
        }
        drop(window);
        worker.join().unwrap();

        let (window, worker) = connect(92);
        let (send, receive) = crossbeam_channel::bounded(1);
        let native = ClientHandle::fixture_restore_on(window, source.clone(), move |wake| {
            if let Wake::CompletedQueue(id, messages) = wake {
                let _ = send.try_send((id, messages));
            }
        })
        .unwrap();
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(5)).unwrap(),
            (id, vec![message.clone()])
        );

        let (window, history_worker) = connect(93);
        let mut history = ControlClient::from_stream(window).unwrap();
        let mut mismatch = source.clone();
        mismatch
            .launch
            .recipe
            .arguments
            .push("different exact launch".into());
        assert!(history.read_exited_chat_queue(mismatch).unwrap().is_none());
        assert_eq!(
            history.read_exited_chat_queue(source.clone()).unwrap(),
            Some(queue)
        );
        history
            .commit_exited_chat_queue(source.clone(), vec!["different-id".into()])
            .unwrap();
        assert!(archive.read(&source, Some(execution)).unwrap().is_some());
        history
            .commit_exited_chat_queue(source.clone(), vec![message.id])
            .unwrap();
        assert!(history.read_exited_chat_queue(source).unwrap().is_none());
        assert_eq!(archive.state.lock().unwrap().bytes, 0);
        assert_eq!(
            host.lock().unwrap().chats[&id].execution_instance,
            replacement
        );
        assert!(!handle.is_completed());
        drop(history);
        history_worker.join().unwrap();
        drop(native);
        worker.join().unwrap();
        handle.close().unwrap();
    }

    #[test]
    fn a_lost_close_reply_does_not_make_the_exact_receipt_unreadable() {
        use crate::{
            agent::Provider,
            chat::{Launch, Session},
            host::state::{HostLock, HostState},
            model::{HostSessionProof, LaunchRecipe},
            terminal_ipc::ProtocolErrorCode,
        };
        use std::sync::Arc;

        let proof = HostSessionProof {
            host_session_id: "completed-archive".into(),
            boot_id: "test-boot".into(),
            host_incarnation: "current".into(),
        };
        let source = Session {
            id: TerminalId::new(),
            launch: Launch {
                provider: Provider::Antigravity,
                session_id: Some("ended-thread".into()),
                recipe: LaunchRecipe {
                    program: "offline-provider".into(),
                    arguments: vec![],
                    working_directory: "/".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                draft_model: None,
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let execution = Uuid::new_v4();
        let queue = ExitedQueue {
            messages: vec![crate::chat::QueuedMessage {
                id: "held-before-exit".into(),
                text: "Keep this after the original socket closes. ".repeat(1024),
                editable: true,
                delivery: Default::default(),
                attachments: vec![],
            }],
            account_scope: None,
            confirmed_session: source.launch.session_id.clone(),
        };
        let host = Arc::new(HostLock::new(HostState::new(proof.clone())));
        let archive = Arc::clone(&host.lock().unwrap().exited_queue_archive);
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        archive
            .retain_in(
                root.path(),
                source.clone(),
                source.clone(),
                execution,
                crate::chat::exited_queue_fingerprint(["held-before-exit"]),
                &encode(&queue).unwrap(),
            )
            .unwrap();
        // The old connection and its record are gone. A new connection reads
        // the exact completed source without depending on the Close reply.
        let recovered = super::super::completed_queue_page(&host, 7, &source, Some(execution))
            .unwrap()
            .unwrap();
        assert_eq!(
            postcard::from_bytes::<ExitedQueue>(&recovered.bytes).unwrap(),
            queue
        );
        // A late completion wake can still name the old execution even when
        // the daemon has installed a different worker at the same pane ID.
        host.lock().unwrap().chats.insert(
            source.id,
            super::super::Record {
                execution_instance: Uuid::new_v4(),
                console_recipe: None,
                paired_terminal: None,
                session: source.clone(),
                handle: crate::chat::runtime::Handle::fixture_failed_remote(),
                source_process: None,
                subscribers: Default::default(),
                observations: Default::default(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        let prior = super::super::completed_queue_page(&host, 7, &source, Some(execution))
            .unwrap()
            .unwrap();
        assert_eq!(
            postcard::from_bytes::<ExitedQueue>(&prior.bytes).unwrap(),
            queue
        );
        host.lock().unwrap().remove_chat(source.id);

        assert!(
            super::super::completed_queue_page(&host, 8, &source, Some(Uuid::new_v4()))
                .unwrap()
                .is_none()
        );
        let mut mismatched = source.clone();
        mismatched.launch.recipe.arguments.push("changed".into());
        assert!(
            super::super::completed_queue_page(&host, 8, &mismatched, None)
                .unwrap()
                .is_none()
        );
        // A delayed acknowledgement for a different set of input IDs
        // must neither reclaim quota nor mark this receipt returned.
        super::super::commit_completed_queue(&host, &source, vec!["older-id".into()]).unwrap();
        assert!(archive.read(&source, Some(execution)).unwrap().is_some());
        for _ in 0..2 {
            super::super::commit_completed_queue(&host, &source, vec!["held-before-exit".into()])
                .unwrap();
        }
        assert!(archive.read(&source, Some(execution)).unwrap().is_none());
        assert_eq!(archive.state.lock().unwrap().bytes, 0);
        let mut newer = queue;
        newer.messages[0].id = "new-input".into();
        let newer_execution = Uuid::new_v4();
        let newer_identity = crate::chat::exited_queue_fingerprint(["new-input"]);
        archive.commit(&source, newer_identity, Some(newer_execution));
        let retain_newer = || {
            archive
                .retain_in(
                    root.path(),
                    source.clone(),
                    source.clone(),
                    newer_execution,
                    newer_identity,
                    &encode(&newer).unwrap(),
                )
                .unwrap();
        };
        retain_newer();
        assert!(
            archive
                .read(&source, Some(newer_execution))
                .unwrap()
                .is_none()
        );
        archive.forget_resident(newer_execution);
        retain_newer();
        super::super::commit_completed_queue(&host, &source, vec!["held-before-exit".into()])
            .unwrap();
        assert!(
            archive
                .read(&source, Some(newer_execution))
                .unwrap()
                .is_some()
        );

        mismatched = source;
        mismatched.proof = Some(HostSessionProof {
            host_incarnation: "foreign".into(),
            ..proof
        });
        assert_eq!(
            super::super::completed_queue_page(&host, 8, &mismatched, None)
                .unwrap_err()
                .code,
            ProtocolErrorCode::ProofMismatch
        );
    }

    #[test]
    fn a_reconnected_resume_gets_a_new_transfer_after_disconnect_or_timeout() {
        let mut transfers = Transfers::default();
        let now = Instant::now();
        let source = TerminalId::new();
        let bytes = || vec![1; EXITED_QUEUE_PAGE_BYTES + 1];
        let first = transfers.begin(1, source, bytes(), now).unwrap();
        transfers.remove(1);
        let next = EXITED_QUEUE_PAGE_BYTES as u32;
        assert!(transfers.read(1, source, first.receipt, next, now).is_err());

        let second = transfers.begin(2, source, bytes(), now).unwrap();
        assert_ne!(second.receipt, first.receipt);
        let expired = now + PAGE_TIMEOUT;
        assert!(
            transfers
                .read(2, source, second.receipt, next, expired)
                .unwrap_err()
                .retryable
        );
        assert!(transfers.active.is_empty());
        let third = transfers.begin(2, source, bytes(), expired).unwrap();
        assert_ne!(third.receipt, second.receipt);
        assert_eq!(third.bytes, first.bytes);
        assert!(
            transfers
                .read(2, source, third.receipt, next, expired)
                .is_ok()
        );
    }
}

/// A completed direct provider can lose its HOST record as soon as Close
/// commits. Keep that exact receipt available even if the Close reply is lost.
/// Anonymous files are private to this daemon and release their storage on exit.
#[derive(Default)]
pub(in crate::host) struct Archive {
    writer: std::sync::Mutex<()>,
    state: std::sync::Mutex<ArchiveState>,
}

#[derive(Default)]
struct ArchiveState {
    entries: Vec<ArchivedTransfer>,
    returned: std::collections::BTreeSet<Uuid>,
    bytes: usize,
}

struct ArchivedTransfer {
    original: crate::chat::Session,
    current: crate::chat::Session,
    execution: Uuid,
    queue_identity: [u8; 32],
    file: std::fs::File,
    size: usize,
}

impl ArchivedTransfer {
    fn names(&self, source: &crate::chat::Session) -> bool {
        &self.original == source || &self.current == source
    }
}

impl Archive {
    pub(super) fn retain(
        &self,
        original: crate::chat::Session,
        current: crate::chat::Session,
        execution: Uuid,
        queue_identity: [u8; 32],
        bytes: &[u8],
    ) -> Result<(), ProtocolError> {
        let paths = crate::daemon::RuntimePaths::discover()
            .map_err(|failure| retryable_unavailable(failure.to_string()))?;
        self.retain_in(
            &paths.directory,
            original,
            current,
            execution,
            queue_identity,
            bytes,
        )
    }

    fn retain_in(
        &self,
        root: &std::path::Path,
        original: crate::chat::Session,
        current: crate::chat::Session,
        execution: Uuid,
        queue_identity: [u8; 32],
        bytes: &[u8],
    ) -> Result<(), ProtocolError> {
        use std::io::Write;
        let _writer = self
            .writer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.returned.contains(&execution)
                || state.entries.iter().any(|entry| {
                    entry.execution == execution
                        && entry.original == original
                        && entry.current == current
                        && entry.queue_identity == queue_identity
                })
            {
                return Ok(());
            }
            if state
                .entries
                .iter()
                .any(|entry| entry.names(&original) || entry.names(&current))
            {
                return Err(retryable_unavailable(
                    "Earlier queued input is still available for this exact conversation",
                ));
            }
            if state.entries.len() >= 128 || state.bytes + bytes.len() > MAX_PENDING_BYTES {
                return Err(retryable_unavailable(
                    "The ended provider could not be closed safely because its queued input archive is full",
                ));
            }
        }
        let mut file = tempfile::tempfile_in(root).map_err(|failure| {
            retryable_unavailable(format!("Could not retain ended provider input: {failure}"))
        })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|failure| {
                retryable_unavailable(format!("Could not retain ended provider input: {failure}"))
            })?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        // A saved checkpoint may arrive while the anonymous file is writing.
        if state.returned.contains(&execution) {
            return Ok(());
        }
        state.bytes += bytes.len();
        state.entries.push(ArchivedTransfer {
            original,
            current,
            execution,
            queue_identity,
            size: bytes.len(),
            file,
        });
        Ok(())
    }

    pub(super) fn returned(&self, execution: Uuid) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .returned
            .contains(&execution)
    }

    pub(in crate::host) fn forget_resident(&self, execution: Uuid) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .returned
            .remove(&execution);
    }

    pub(super) fn commit(
        &self,
        source: &crate::chat::Session,
        queue_identity: [u8; 32],
        resident: Option<Uuid>,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut reclaimed = 0;
        state.entries.retain(|entry| {
            if entry.names(source) && entry.queue_identity == queue_identity {
                reclaimed += entry.size;
                return false;
            }
            true
        });
        state.bytes -= reclaimed;
        if let Some(execution) = resident {
            state.returned.insert(execution);
        }
    }

    pub(super) fn read(
        &self,
        source: &crate::chat::Session,
        execution: Option<Uuid>,
    ) -> Result<Option<Vec<u8>>, ProtocolError> {
        use std::os::unix::fs::FileExt;
        let (file, size) = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let Some(entry) = state.entries.iter().find(|entry| {
                entry.names(source)
                    && execution.is_none_or(|execution| execution == entry.execution)
            }) else {
                return Ok(None);
            };
            let file = entry.file.try_clone().map_err(|failure| {
                retryable_unavailable(format!("Could not open retained queued input: {failure}"))
            })?;
            (file, entry.size)
        };
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| retryable_unavailable("Could not allocate the retired input queue"))?;
        bytes.resize(size, 0);
        file.read_exact_at(&mut bytes, 0).map_err(|failure| {
            retryable_unavailable(format!("Could not read retained queued input: {failure}"))
        })?;
        Ok(Some(bytes))
    }

    pub(super) fn prior_execution(
        &self,
        requested: &crate::chat::Session,
        attached: &crate::chat::Session,
        resident: Uuid,
    ) -> Option<Uuid> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entries
            .iter()
            .find(|entry| {
                entry.execution != resident && entry.names(requested) && entry.names(attached)
            })
            .map(|entry| entry.execution)
    }
}
