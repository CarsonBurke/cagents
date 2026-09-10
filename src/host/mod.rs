mod chat;
mod client;
mod delivery;
mod state;

use std::{
    fs, io,
    net::Shutdown,
    os::unix::fs::PermissionsExt,
    os::unix::net::UnixListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
};

use thiserror::Error;
use uuid::Uuid;

use crate::{
    daemon::{self, HostLease, PlatformError, RuntimePaths},
    model::{HostIncarnation, HostSessionId, HostSessionProof},
    terminal_ipc::{PROTOCOL_VERSION, ServerEnvelope, ServerMessage},
};

use self::state::{HostState, SharedHost};

// Together with MAX_CLIENT_FRAME_BYTES this caps hostile in-flight request
// payloads at 256 MiB, while ordinary GUI/CLI fan-out remains ample.
const MAX_ACTIVE_CLIENTS: usize = 32;

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

pub fn run() -> Result<(), HostError> {
    daemon::install_signal_handlers()?;
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new(Uuid::new_v4().to_string()),
        boot_id: daemon::read_boot_id()?,
        host_incarnation: HostIncarnation::new(Uuid::new_v4().to_string()),
    };
    let paths = RuntimePaths::discover()?;
    let _lease = HostLease::acquire(&paths, &proof)?;
    let listener = UnixListener::bind(&paths.socket)?;
    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let _tray = crate::tray::start();

    let host: SharedHost = Arc::new(Mutex::new(HostState::new(proof)));
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
                    let _ = stream.shutdown(Shutdown::Both);
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
    let proof = host
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .proof()
        .clone();
    let (handles, deliveries) = host
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .graceful_shutdown();
    let chats = host
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .chats
        .values()
        .map(|record| record.handle.clone())
        .collect::<Vec<_>>();
    for chat in chats {
        if let Err(error) = chat.close() {
            tracing::warn!(%error, "provider shutdown was not confirmed");
        }
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
}
