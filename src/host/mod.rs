mod activity;
mod agent_execution;
mod automations;
mod chat;
mod client;
mod delivery;
pub(crate) mod descriptor_limit;
mod lock;
mod model;
mod notifications;
mod state;

use std::{
    fs, io,
    net::Shutdown,
    os::unix::fs::PermissionsExt,
    os::unix::net::{UnixListener, UnixStream},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
};

use thiserror::Error;
use uuid::Uuid;

use crate::{
    daemon::{self, HostLease, PlatformError, RuntimePaths},
    model::{HostIncarnation, HostSessionId, HostSessionProof},
    terminal_ipc::{
        PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ServerEnvelope, ServerMessage,
        write_frame,
    },
};

use self::state::{HostState, SharedHost};

// A card, a chat and every CLI or tray invocation each hold their own
// connection for its lifetime, so this must never be the limit that binds
// before `MAX_TERMINALS`: at 32 the 33rd card was refused with a bare EOF and
// reported a lost daemon. A connection costs a reader thread, a writer thread,
// one `DEFAULT_CLIENT_QUEUE_CAPACITY` queue that is empty while idle, and up to
// MAX_CLIENT_FRAME_BYTES of read scratch while a request is in flight. Two
// connections per terminal plus headroom for CLI and tray clients bounds the
// steady-state cost at a few thousand mostly-parked threads; the peer is
// already restricted to this uid, so the frame-scratch ceiling is a resource
// bound on the user's own processes, not a trust boundary.
const MAX_ACTIVE_CLIENTS: usize = 2 * state::MAX_TERMINALS + 64;
// The terminal limit, not the connection limit, must be what an operator hits.
const _: () = assert!(MAX_ACTIVE_CLIENTS > state::MAX_TERMINALS);

struct ClientCapacity {
    active: Arc<AtomicUsize>,
    limit: usize,
}

impl ClientCapacity {
    fn new(limit: usize) -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            limit,
        }
    }

    fn try_acquire(&self) -> Option<ClientPermit> {
        let mut active = self.active.load(Ordering::Acquire);
        loop {
            if active >= self.limit {
                return None;
            }
            match self.active.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(ClientPermit {
                        active: Arc::clone(&self.active),
                    });
                }
                Err(observed) => active = observed,
            }
        }
    }
}

struct ClientPermit {
    active: Arc<AtomicUsize>,
}

impl Drop for ClientPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Release);
    }
}

fn reap_finished(clients: &mut Vec<JoinHandle<()>>) {
    let mut index = 0;
    while index < clients.len() {
        if clients[index].is_finished() {
            let handle = clients.swap_remove(index);
            if handle.join().is_err() {
                tracing::warn!("cagents daemon client thread panicked");
            }
        } else {
            index += 1;
        }
    }
}

/// A refused peer is not reading yet; never let it stall the accept loop.
const REFUSAL_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Tells a peer refused for capacity why, before hanging up.
///
/// Closing the socket without a frame made a capacity refusal
/// indistinguishable from a dead daemon: the client retried, timed out, and
/// reported that the daemon connection had been lost.
fn refuse_at_capacity(host: &SharedHost, mut stream: UnixStream, capacity: &ClientCapacity) {
    tracing::warn!(
        active = capacity.active.load(Ordering::Acquire),
        limit = capacity.limit,
        "refused cagents daemon client at capacity"
    );
    let proof = host
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .proof()
        .clone();
    if stream
        .set_write_timeout(Some(REFUSAL_WRITE_TIMEOUT))
        .is_ok()
    {
        let _ = write_frame(
            &mut stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: None,
                proof,
                message: ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::QueueFull,
                    format!(
                        "the cagents daemon already serves its maximum of {} client connections; \
                         close some cards or chats before opening another",
                        capacity.limit
                    ),
                    false,
                )),
            },
        );
    }
    let _ = stream.shutdown(Shutdown::Both);
}

#[derive(Debug, Error)]
pub enum HostError {
    #[error("cagents daemon platform setup failed: {0}")]
    Platform(String),
    #[error("cagents daemon I/O failed: {0}")]
    Io(#[from] io::Error),
}

impl From<PlatformError> for HostError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error.to_string())
    }
}

/// Dedicated precheck owners must enter before tracing or runtime threads are
/// started: their descendant cleanup relies on being the sole child reaper.
pub fn run_automation_helper() -> Option<Result<(), String>> {
    automations::helper_entry()
}

pub fn run() -> Result<(), HostError> {
    daemon::install_signal_handlers()?;
    descriptor_limit::raise();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new(Uuid::new_v4().to_string()),
        boot_id: daemon::read_boot_id()?,
        host_incarnation: HostIncarnation::new(Uuid::new_v4().to_string()),
    };
    let paths = RuntimePaths::discover()?;
    let _lease = HostLease::acquire(&paths, &proof)?;
    let host: SharedHost = Arc::new(crate::host::state::HostLock::new(HostState::new(proof)));
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .enable_holder_recovery();
    // A daemon that cannot open the shared-model store still hosts terminals.
    // The model is reported unavailable to the clients that ask for it, which is
    // what keeps a window from mistaking a broken store for an empty registry
    // and donating its state over and over.
    match model::Registry::open() {
        Ok(registry) => host
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .install_model(registry),
        Err(error) => {
            tracing::error!(%error, "the shared workspace model is unavailable; channels will \
                                    not be shared between windows")
        }
    }
    let notification_application = crate::instance::notification_daemon_application(
        &std::env::current_exe()?,
        std::env::args_os()
            .any(|argument| argument == crate::instance::CEDITOR_NOTIFICATION_ARGUMENT),
    )?;
    let notification_broker = match notifications::Broker::start(&host, notification_application) {
        Ok(broker) => Some(broker),
        Err(error) => {
            tracing::warn!(%error, "agent notifications are unavailable");
            None
        }
    };
    if let Some(broker) = &notification_broker {
        host.lock().unwrap_or_else(|p| p.into_inner()).notifications = Some(broker.handle());
    }
    let unattached = state::restore_upgrade(&host).map_err(io::Error::other)?;
    state::restore_orphaned_holders(&host).map_err(io::Error::other)?;
    chat::restore_orphaned_native(&host, &unattached).map_err(io::Error::other)?;
    // A committed acceptance is not proof of provider delivery. Until a
    // restored actor supplies exact run evidence, never authorize a resend.
    if let Ok(registry) = agent_execution::registry(&host) {
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .interrupt_unproven_agent_runs(crate::chat::now_ms().unwrap_or(0))
            .map_err(io::Error::other)?;
    }
    let _agent_activity = activity::Monitor::start(&host)?;
    let listener = UnixListener::bind(&paths.socket)?;
    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let tally_host = Arc::downgrade(&host);
    let _tray = crate::tray::start(move || {
        let host = tally_host.upgrade()?;
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        Some(state.agent_tally())
    });

    let automation_broker = match automations::start(&host) {
        Ok(broker) => Some(broker),
        Err(error) => {
            tracing::warn!(%error, "scheduled automations are unavailable");
            None
        }
    };
    let courier = match agent_execution::mailbox::Courier::start(&host) {
        Ok(courier) => Some(courier),
        Err(error) => {
            tracing::warn!(%error, "messages between sessions are unavailable");
            None
        }
    };

    // Restoring a checkpoint above may hold the host for as long as its
    // inventory takes; from here on a hold that long is a deadlock.
    let stall_watch = match host.watch(lock::STALL_LIMIT) {
        Ok(watch) => Some(watch),
        Err(error) => {
            tracing::warn!(%error, "a wedged host mutex will not be detected");
            None
        }
    };

    let next_client_id = AtomicU64::new(1);
    let client_capacity = ClientCapacity::new(MAX_ACTIVE_CLIENTS);
    let mut clients = Vec::new();
    let mut runtime_error = None;
    tracing::info!(
        instance = crate::instance::NAME,
        socket = %paths.socket.display(),
        "cagents daemon is ready"
    );

    loop {
        reap_finished(&mut clients);
        if daemon::termination_requested() {
            host.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .request_shutdown();
        }
        if host
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_shutting_down()
        {
            break;
        }

        let fatal = match listener.accept() {
            Ok((stream, _address)) => {
                match daemon::peer_uid(&stream) {
                    Ok(uid) if uid == daemon::current_uid() => {}
                    Ok(uid) => {
                        tracing::warn!(peer_uid = uid, "rejected cagents daemon peer");
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not authenticate cagents daemon peer");
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                }
                let Some(permit) = client_capacity.try_acquire() else {
                    refuse_at_capacity(&host, stream, &client_capacity);
                    continue;
                };
                let client_id = next_client_id.fetch_add(1, Ordering::Relaxed);
                let client_host = Arc::clone(&host);
                match thread::Builder::new()
                    .name(format!("cagentsd-client-{client_id}"))
                    .spawn(move || {
                        client::serve(stream, client_id, client_host);
                        // Free the slot before the wakeup byte, so the loop this
                        // wakes cannot refuse the connection the wakeup exists to
                        // admit, and can reap this thread without a timer.
                        drop(permit);
                        daemon::wake();
                    }) {
                    Ok(client) => clients.push(client),
                    Err(error) => tracing::warn!(%error, "could not start cagents daemon client"),
                }
                None
            }
            // Nothing is pending. Park until a peer connects, a client thread
            // exits, or a signal arrives; an idle daemon blocks here indefinitely
            // instead of waking on a timer.
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                daemon::wait_for_activity(&listener).err()
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => None,
            Err(error) => Some(error),
        };
        if let Some(error) = fatal {
            host.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .request_shutdown();
            runtime_error = Some(error);
            break;
        }
    }

    tracing::info!("cagents daemon is shutting down");
    // Shutdown waits on providers and holders by design.
    drop(stall_watch);
    // Stop deadline acceptance before snapshotting/tearing down provider
    // ownership. Dedicated prechecks keep their own bounded cleanup deadline.
    drop(automation_broker);
    drop(courier);
    let proof = host
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .proof()
        .clone();
    let (handles, deliveries) = host
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .graceful_shutdown();
    if let Err(error) = finish_chat_shutdown(&host) {
        tracing::error!(%error, "upgrade handoff remains unconfirmed; checkpoint retained");
        runtime_error = Some(io::Error::other(error));
    }
    for delivery in &deliveries {
        let _ = delivery.push(ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: None,
            proof: proof.clone(),
            message: ServerMessage::HostShuttingDown,
        });
        delivery.close_after_drain();
    }
    for handle in handles {
        let _ = handle.shutdown();
    }
    for client in clients {
        let _ = client.join();
    }
    match runtime_error {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

fn finish_chat_shutdown(host: &SharedHost) -> Result<(), String> {
    let (chats, upgrading) = {
        let state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (
            state
                .chats
                .values()
                .map(|record| record.handle.clone())
                .collect::<Vec<_>>(),
            state.has_committed_upgrade(),
        )
    };
    let result = state::finish_upgrade(host);
    for chat in chats {
        // A committed upgrade transfers ownership, including when the final
        // checkpoint write fails. Explicit Stop/Close still tears down the
        // provider; exiting the upgrading daemon must not undo preservation.
        let closed = if upgrading
            && matches!(
                chat.native_continuity(),
                crate::chat::NativeContinuity::Durable { .. }
            ) {
            chat.preserve_for_restart()
        } else {
            chat.close()
        };
        if let Err(error) = closed {
            tracing::warn!(%error, "provider shutdown was not confirmed");
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_capacity_is_atomic_and_released_by_permit_drop() {
        let capacity = ClientCapacity::new(2);
        let first = capacity.try_acquire().unwrap();
        let second = capacity.try_acquire().unwrap();
        assert!(capacity.try_acquire().is_none());
        assert_eq!(capacity.active.load(Ordering::Acquire), 2);

        drop(first);
        let replacement = capacity.try_acquire().unwrap();
        assert_eq!(capacity.active.load(Ordering::Acquire), 2);

        drop(second);
        drop(replacement);
        assert_eq!(capacity.active.load(Ordering::Acquire), 0);
    }

    /// A refusal must be readable as a protocol error. Shutting the socket
    /// without a frame made a capacity limit indistinguishable from a dead
    /// daemon, and the client reported a lost connection instead.
    #[test]
    fn a_refused_client_reads_a_capacity_error_before_the_socket_closes() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let host: SharedHost = Arc::new(crate::host::state::HostLock::new(HostState::new(
            HostSessionProof {
                host_session_id: HostSessionId::new("session"),
                boot_id: crate::model::BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            },
        )));
        let capacity = ClientCapacity::new(1);
        let _permit = capacity.try_acquire().unwrap();
        assert!(capacity.try_acquire().is_none());

        refuse_at_capacity(&host, server, &capacity);

        let envelope = crate::terminal_ipc::read_frame::<ServerEnvelope>(&mut client)
            .unwrap()
            .unwrap();
        assert_eq!(envelope.request_id, None);
        let ServerMessage::Error(error) = envelope.message else {
            panic!("a refused client was not told why");
        };
        assert_eq!(error.code, ProtocolErrorCode::QueueFull);
        assert!(!error.retryable);
        assert!(error.message.contains("client connections"));
        assert!(
            crate::terminal_ipc::read_frame::<ServerEnvelope>(&mut client)
                .unwrap()
                .is_none()
        );
    }
}
