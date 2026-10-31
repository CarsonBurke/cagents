mod account_lifecycle;
pub(crate) use account_lifecycle::Reservation as AccountReservation;
mod activity;
mod agent_execution;
mod automations;
mod channel_work;
mod chat;
mod client;
mod delivery;
pub(crate) mod descriptor_limit;
#[cfg(all(test, target_os = "linux"))]
mod exit_drain_tests;
mod lock;
mod model;
mod notifications;
#[cfg(all(test, target_os = "linux"))]
mod provider_shutdown_tests;
mod service;
#[cfg(all(test, target_os = "linux"))]
mod shutdown_tests;
mod state;
mod tray_attention;
mod work;
mod worktree_removal;

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

/// Private retained-provider owner mode. Invoke before ordinary daemon setup;
/// its captured account context and cleanup status belong to the launch owner.
pub fn run_retained_actor_helper() -> Option<Result<(), String>> {
    crate::chat::runtime::run_retained_helper_if_requested()
}

/// Dedicated precheck owners must enter before tracing or runtime threads are
/// started: their descendant cleanup relies on being the sole child reaper.
pub fn run_automation_helper() -> Option<Result<(), String>> {
    automations::helper_entry()
}

mod upgrade_inspect;
pub fn run_upgrade_inspection_helper() -> Option<Result<(), String>> {
    upgrade_inspect::entry()
}

/// Holds launch exclusion until the daemon listener is ready.
pub struct DaemonAdmission {
    _guard: crate::instance::DaemonAdmissionGuard,
}

/// Authenticate startup authority before diagnostics or runtime threads exist.
///
/// # Safety
/// Call from the single-threaded entry point: this clears inherited authority
/// markers so later terminal children cannot inherit a consumed descriptor.
pub unsafe fn prepare_daemon_admission() -> Result<DaemonAdmission, HostError> {
    let admission = crate::instance::DaemonAdmissionGuard::acquire()?;
    // SAFETY: the caller guarantees that no other thread accesses environment.
    unsafe {
        std::env::remove_var(crate::instance::DAEMON_FENCE_ENV);
        std::env::remove_var(crate::instance::INSTALL_GUARD_ENV);
    }
    Ok(DaemonAdmission { _guard: admission })
}

/// Reserve and configure the serving socket before restore can take ownership
/// of an original holder. The caller still holds HostLease and admission, and
/// does not accept clients or publish readiness until restoration finishes.
fn listener_before_restore(
    paths: &RuntimePaths,
    restore: impl FnOnce() -> Result<(), HostError>,
) -> Result<UnixListener, HostError> {
    let listener = UnixListener::bind(&paths.socket)?;
    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    restore()?;
    Ok(listener)
}

pub fn run(admission: DaemonAdmission) -> Result<(), HostError> {
    daemon::install_signal_handlers()?;
    descriptor_limit::raise();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new(Uuid::new_v4().to_string()),
        boot_id: daemon::read_boot_id()?,
        host_incarnation: HostIncarnation::new(Uuid::new_v4().to_string()),
    };
    let paths = RuntimePaths::discover()?;
    let _lease = HostLease::acquire(&paths, &proof)?;
    let mut initial = HostState::new(proof);
    initial.native_chat_capacity = crate::native_chat_capacity::load()? as usize;
    let host: SharedHost = Arc::new(crate::host::state::HostLock::new(initial));
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
    // Cache sweeps preserve unretired helper journals and live Store leases.
    if let Err(error) = crate::transcript_cache::sweep(&paths.directory) {
        tracing::warn!(%error, "transcript cache cleanup was refused; evidence preserved");
    }
    // This mandatory thread service must be available before any adoption
    // can discard the holder's replay copy. It observes no process until the
    // same HostState publishes one; it initiates no execution.
    let _agent_activity = activity::Monitor::start(&host)?;
    let listener = listener_before_restore(&paths, || {
        let restored = (|| -> io::Result<()> {
            chat::retained_actor::reconcile_previous_boot_history(&host)
                .map_err(io::Error::other)?;
            let unattached = state::restore_upgrade(&host).map_err(io::Error::other)?;
            state::restore_orphaned_holders(&host).map_err(io::Error::other)?;
            if let Some(detail) = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .startup_recovery()
            {
                return Err(io::Error::other(detail.to_owned()));
            }
            chat::restore_orphaned_native(&host, &unattached).map_err(io::Error::other)?;
            chat::retained_actor::reconcile_retired_actors(&host).map_err(io::Error::other)?;
            chat::retained_actor::reconcile_startup_losses(&host).map_err(io::Error::other)?;
            let retained_runs = chat::retained_actor::retained_runs(&host);
            // A committed acceptance is not proof of provider delivery. Until a
            // restored actor supplies exact run evidence, never authorize a resend.
            if let Ok(registry) = agent_execution::registry(&host) {
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .interrupt_unproven_agent_runs_except(
                        crate::chat::now_ms().unwrap_or(0),
                        &retained_runs,
                    )
                    .map_err(io::Error::other)?;
            }
            Ok(())
        })();
        match restored {
            Ok(()) => Ok(()),
            Err(error) => {
                let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                if state
                    .terminals
                    .values()
                    .any(|record| !record.handle.is_completed())
                    || state.chats.values().any(|record| record.handle.owns_work())
                {
                    // Keep the real listener, lease and original owners. A
                    // failed restore is not permission to destroy accepted
                    // replay or start an empty replacement daemon.
                    state.retain_startup_recovery(error.to_string());
                    tracing::error!(%error, "startup retained original ownership; recovery-only listener remains available");
                    Ok(())
                } else {
                    Err(error.into())
                }
            }
        }
    })?;
    drop(admission);
    crate::diagnostics::record(crate::diagnostics::Event::Ready);
    let _tray = tray_attention::start(&host, notification_application);

    let recovery_only = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .startup_recovery()
        .is_some();
    let mut automation_broker = if recovery_only {
        None
    } else {
        match automations::start(&host) {
            Ok(broker) => Some(broker),
            Err(error) => {
                tracing::warn!(%error, "scheduled automations are unavailable");
                None
            }
        }
    };
    let mut work_board = if recovery_only {
        None
    } else {
        match work::Service::start(&host) {
            Ok(service) => {
                host.lock().unwrap_or_else(|p| p.into_inner()).work = Some(service.handle());
                Some(service)
            }
            Err(error) => {
                tracing::warn!(%error, "the Work board is unavailable");
                None
            }
        }
    };
    let mut courier = if recovery_only {
        None
    } else {
        match agent_execution::mailbox::Courier::start(&host) {
            Ok(courier) => Some(courier),
            Err(error) => {
                tracing::warn!(%error, "messages between sessions are unavailable");
                None
            }
        }
    };

    // Restoring a checkpoint above may hold the host for as long as its
    // inventory takes; from here on a hold that long is a deadlock.
    let mut stall_watch = match host.watch(lock::STALL_LIMIT) {
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
    let mut ordinary_exit_drain: Option<crate::daemon::child_reaper::ExitDrain> = None;
    let mut ordinary_cleanup_started = false;
    let mut sealed_deliveries = None;
    tracing::info!(
        instance = crate::instance::NAME,
        socket = %paths.socket.display(),
        "cagents daemon is ready"
    );

    #[cfg(all(test, target_os = "linux"))]
    exit_drain_tests::observe_ready(&host);
    loop {
        reap_finished(&mut clients);
        if daemon::termination_requested() {
            host.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .request_shutdown();
        }
        // Read the epoch BEFORE checking absence. A final ticket release
        // between this check and a fatal-listener wait must not be lost.
        let progress_epoch = ordinary_exit_drain
            .as_ref()
            .map(|drain| drain.progress_epoch());
        if let Some(drain) = &ordinary_exit_drain {
            if !ordinary_cleanup_started {
                ordinary_cleanup_started = finish_shutdown_owners(&host, &mut runtime_error, false);
            }
            // Restored originals can lack a local reaper credit. Their
            // independent parent/startup proof must settle before either
            // ordinary teardown or the terminal-child absence seal authorizes
            // returning from the daemon.
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if ordinary_cleanup_started
                && !state.ordinary_exit_cleanup_blocked()
                && drain.try_seal_absence()
            {
                // The startup/reply guard, core seal and registration/reply
                // acquisition fence share this one HostMutex transaction.
                sealed_deliveries = Some(state.close_ordinary_exit_registration());
                break;
            }
        } else if host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_shutting_down()
        {
            let Some(drain) = begin_ordinary_exit(&host) else {
                break; // saved committed-successor decision, before teardown
            };
            ordinary_exit_drain = Some(drain);

            // Stop autonomous producers once, then use the original ordinary
            // cleanup paths. No producer/reaper waits while holding control.
            drop(stall_watch.take());
            drop(automation_broker.take());
            drop(courier.take());
            host.lock().unwrap_or_else(|p| p.into_inner()).work = None;
            drop(work_board.take());
            ordinary_cleanup_started = finish_shutdown_owners(&host, &mut runtime_error, false);
            tracing::info!(pending = %ordinary_exit_drain.as_ref().unwrap().pending_description(),
                "ordinary daemon exit retains its listener and lease until exact child absence is sealed");
            continue;
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
            if host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .ordinary_exit_cleanup_blocked()
            {
                // Preserve 1204's same-listener recovery on a broken socket.
                // A restored owner may have no reaper epoch to wait for.
                tracing::error!(%error, "recovery listener failed while original ownership remains retained");
                runtime_error = Some(error);
                thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            if let Some(drain) = &ordinary_exit_drain {
                tracing::error!(%error, pending = %drain.pending_description(),
                    "listener unavailable while original child custody remains; retaining daemon and lease");
                runtime_error = Some(error);
                drain.wait_for_progress(progress_epoch.expect("active ordinary exit epoch"));
                continue;
            }
            host.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .request_shutdown();
            runtime_error = Some(error);
            // Arm ordinary absence before any return/drop; a committed
            // successor transfer still takes the saved bypass above.
            continue;
        }
    }

    tracing::info!("cagents daemon is shutting down");
    if ordinary_exit_drain.is_some() {
        // Already fenced atomically with the final startup/core absence check.
        // Late accepted threads receive an already-closed queue, while the
        // original writer drains every correlated checked-Close reply first.
        let deliveries = sealed_deliveries.expect("ordinary exit sealed its delivery fence");
        close_shutdown_deliveries(&host, &deliveries);
    } else {
        // The committed-successor path keeps its existing intentional transfer
        // behavior and never seals absence or closes reaper admission.
        drop(stall_watch);
        drop(automation_broker);
        drop(courier);
        host.lock().unwrap_or_else(|p| p.into_inner()).work = None;
        drop(work_board);
        finish_shutdown_owners(&host, &mut runtime_error, true);
    }
    for client in clients {
        let _ = client.join();
    }
    // Healthy originals deliberately handed to a successor may outlive us.
    // A suspended or failed handoff must not be reported as proved cleanup.
    if let Some(error) = crate::daemon::child_reaper::retained_custody_error() {
        tracing::error!(%error, "daemon shutdown retained unconfirmed original child custody");
        runtime_error.get_or_insert_with(|| io::Error::other(error));
    }
    match runtime_error {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

/// The ordinary exit caller owns the permit. Keep reservation closure and
/// public quiescence under the existing writer, then release both locks before
/// invoking any producer Drop, provider cleanup, child wait or client work.
fn begin_ordinary_exit(host: &SharedHost) -> Option<crate::daemon::child_reaper::ExitDrain> {
    let gate = Arc::clone(&host.lock().unwrap_or_else(|p| p.into_inner()).control_gate);
    let _control = gate.write().unwrap_or_else(|p| p.into_inner());
    // Save intentional successor transfer BEFORE finish_upgrade takes its
    // state. Such a handoff is not a claim of original-child absence.
    if host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .has_committed_upgrade()
    {
        return None;
    }
    let drain = crate::daemon::child_reaper::begin_exit_drain();
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .begin_ordinary_exit_cleanup();
    Some(drain)
}

fn close_shutdown_deliveries(host: &SharedHost, deliveries: &[Arc<delivery::DeliveryQueue>]) {
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    for delivery in deliveries {
        let _ = delivery.push(ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: None,
            proof: proof.clone(),
            message: ServerMessage::HostShuttingDown,
        });
        delivery.close_after_drain();
    }
}

/// Ordinary cleanup runs exactly once before waiting for original tickets.
/// Successor cleanup preserves the original notification-before-terminal order.
fn finish_shutdown_owners(
    host: &SharedHost,
    runtime_error: &mut Option<io::Error>,
    close_deliveries: bool,
) -> bool {
    let (handles, deliveries) = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if !close_deliveries && state.ordinary_exit_cleanup_blocked() {
            return false;
        }
        state.graceful_shutdown()
    };
    if let Err(error) = finish_chat_shutdown(host) {
        tracing::error!(%error, "daemon shutdown left unconfirmed ownership; recovery evidence retained");
        *runtime_error = Some(io::Error::other(error));
    }
    if close_deliveries {
        close_shutdown_deliveries(host, &deliveries);
    }
    for result in finish_terminal_shutdown(&handles) {
        if let Err(error) = result {
            tracing::error!(%error, "daemon terminal shutdown left unconfirmed ownership; recovery evidence retained");
            *runtime_error = Some(io::Error::other(format!(
                "terminal shutdown left unconfirmed ownership: {error}"
            )));
        }
    }
    true
}

fn finish_terminal_shutdown(
    handles: &[crate::terminal::TerminalHandle],
) -> Vec<Result<(), crate::terminal::TerminalSendError>> {
    // Request and register every original before waiting on any one worker.
    // One absolute deadline bounds the whole batch, including pending joins;
    // an unfinished owner cannot consume another five seconds per sibling.
    let deadline = std::time::Instant::now() + crate::terminal::TerminalHandle::SHUTDOWN_TIMEOUT;
    for handle in handles {
        handle.request_shutdown();
    }
    handles
        .iter()
        .map(|handle| handle.shutdown_until(deadline))
        .collect()
}

fn finish_chat_shutdown(host: &SharedHost) -> Result<(), String> {
    let (chats, upgrading) = {
        let state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (
            state
                .chats
                .values()
                .map(|record| (record.session.clone(), record.handle.clone()))
                .collect::<Vec<_>>(),
            state.has_committed_upgrade(),
        )
    };
    let mut result = state::finish_upgrade(host);
    for (session, chat) in chats {
        // A committed upgrade transfers ownership, including when the final
        // checkpoint write fails. Explicit Stop/Close still tears down the
        // provider; exiting the upgrading daemon must not undo preservation.
        let closed = if upgrading && chat.native_continuity().retains_provider() {
            chat.preserve_for_restart()
        } else {
            chat.close().and_then(|()| {
                if !upgrading {
                    // Closing the provider leaves its retained helper alive
                    // until final outcomes are durably committed and ACKed.
                    // Preserve that ownership on failure, before retiring the
                    // delegated session record below.
                    chat::retained_actor::finish_close(host, &chat, &session)
                } else {
                    Ok(())
                }
            })
        };
        match closed {
            Ok(()) if !upgrading => {
                // Ordinary shutdown proved this exact provider stopped. Retire
                // its delegated record before a new daemon can resume the UI
                // descriptor with another proof; absence alone cannot do this.
                if let Err(error) = agent_execution::retired(host, &session) {
                    tracing::warn!(%error, "delegated provider retirement was not saved");
                    if result.is_ok() {
                        result = Err(error);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "provider shutdown was not confirmed");
                if result.is_ok() {
                    result = Err(format!(
                        "Provider {} shutdown was not confirmed: {error}",
                        session.id
                    ));
                }
            }
            Ok(()) => {}
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
