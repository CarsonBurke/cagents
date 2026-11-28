mod account_lifecycle;
#[cfg(test)]
mod account_remote_tests;
#[cfg(all(test, target_os = "linux"))]
mod account_removal_contract_tests;
#[cfg(test)]
mod account_restore_tests;
#[cfg(test)]
pub(crate) use account_lifecycle::Reservation as AccountReservation;
mod activity;
mod agent_execution;
mod automations;
mod chat;
mod client;
mod control_gate;
mod delivery;
mod device_stats;
mod device_trust;
#[cfg(all(test, target_os = "linux"))]
mod exit_drain_tests;
mod file_revision;
#[cfg(debug_assertions)]
mod fixture_close_refusal;
#[cfg(debug_assertions)]
mod fixture_hold;
#[cfg(debug_assertions)]
mod fixture_startup_recovery;
mod lifecycle;
mod lock;
mod model;
mod notifications;
#[cfg(all(test, target_os = "linux"))]
mod provider_shutdown_tests;
mod provider_status;
mod recovery_admission;
mod service;
#[cfg(all(test, target_os = "linux"))]
mod shutdown_tests;
mod state;
mod update_recovery;
mod work;
mod workspace;
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

use crate::{
    daemon::{self, HostLease, PlatformError, RuntimePaths},
    terminal_ipc::{
        PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ServerEnvelope, ServerMessage,
        write_frame,
    },
};

use self::state::HostState;
pub(crate) use self::state::SharedHost;

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

fn accept_client(
    listener: &UnixListener,
    host: &SharedHost,
    next_client_id: &AtomicU64,
    capacity: &ClientCapacity,
    clients: &mut Vec<JoinHandle<()>>,
) -> io::Result<()> {
    let (stream, _peer_address) = crate::descriptor::accept(listener)?;
    match daemon::peer_uid(&stream) {
        Ok(uid) if uid == daemon::current_uid() => {}
        Ok(uid) => {
            tracing::warn!(peer_uid = uid, "rejected cagents daemon peer");
            let _ = stream.shutdown(Shutdown::Both);
            return Ok(());
        }
        Err(error) => {
            tracing::warn!(%error, "could not authenticate cagents daemon peer");
            let _ = stream.shutdown(Shutdown::Both);
            return Ok(());
        }
    }
    let Some(permit) = capacity.try_acquire() else {
        refuse_at_capacity(host, stream, capacity);
        return Ok(());
    };
    let client_id = next_client_id.fetch_add(1, Ordering::Relaxed);
    let client_host = Arc::clone(host);
    let serve = client::serve;
    #[cfg(all(test, target_os = "linux"))]
    let serve = if exit_drain_tests::take_fixture_client(&_peer_address) {
        client::serve_fixture_user
    } else {
        serve
    };
    match thread::Builder::new()
        .name(format!("cagentsd-client-{client_id}"))
        .spawn(move || {
            serve(stream, client_id, client_host);
            // Free the slot before the wake, so its next peer can be admitted.
            drop(permit);
            daemon::wake();
        }) {
        Ok(client) => clients.push(client),
        Err(error) => tracing::warn!(%error, "could not start cagents daemon client"),
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum HostError {
    #[error("cagents daemon platform setup failed: {0}")]
    Platform(String),
    #[error("cagents daemon I/O failed: {0}")]
    Io(#[from] io::Error),
    /// Another runtime directory's daemon serves this instance's saved data.
    #[error("{0}")]
    StateInUse(crate::instance::state_claim::StateInUse),
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

/// Fills this instance's empty workspace-model store from a fixture plan and
/// returns what it wrote as one JSON line. Refused by production builds and
/// while a daemon runs. See docs/fixture-workspace.md.
pub fn seed_fixture_workspace(plan: &std::path::Path) -> Result<String, String> {
    #[cfg(any(debug_assertions, feature = "development"))]
    {
        let seeded = model::fixture_seed::seed(plan)?;
        serde_json::to_string(&seeded).map_err(|error| error.to_string())
    }
    #[cfg(not(any(debug_assertions, feature = "development")))]
    {
        let _ = plan;
        Err(crate::fixture_workspace::refusal())
    }
}

mod upgrade_inspect;
pub fn run_upgrade_inspection_helper() -> Option<Result<(), String>> {
    upgrade_inspect::entry()
}

/// Holds launch exclusion until the daemon listener is ready.
pub struct DaemonAdmission {
    _guard: crate::instance::DaemonAdmissionGuard,
    lease: Option<std::fs::File>,
    retirement_owned_by_updater: bool,
    standby: Option<daemon::standby::Activated>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct StartupPublication {
    publication: Option<crate::update::LocalPublication>,
    _admission: DaemonAdmission,
    #[cfg(target_os = "macos")]
    recovered: Option<InstalledRestoration>,
}

#[cfg(target_os = "macos")]
enum InstalledRestoration {
    Prepared(crate::upgrade::recovery::RecoveryRecord),
    Ready {
        admission: crate::upgrade::recovery::ReadyAdmission,
        restored: state::RestoredUpgrade,
    },
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish_restored_selection(host: &SharedHost, pending: &mut Option<StartupPublication>) {
    if pending.is_none() {
        return;
    }
    use crate::terminal_ipc::StartupState;
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let readiness = state.startup_state_before_publication();
    if matches!(readiness, StartupState::Restoring { .. }) {
        return;
    }
    let publication = pending.take().unwrap();
    // Linux keeps its established matched inspection route for retained work.
    #[cfg(target_os = "linux")]
    let result = publication
        .publication
        .expect("Linux publication requires a prepared selector")
        .publish()
        .map_err(|error| error.to_string());
    #[cfg(target_os = "macos")]
    let result = (|| {
        if readiness != StartupState::Ready {
            return Err("The replacement retained the original owners for inspection".into());
        }
        let recovered = publication
            .recovered
            .ok_or("The replacement did not settle the original checkpoint")?;
        let terminals = state.list(0);
        match &recovered {
            InstalledRestoration::Prepared(record) => {
                let chats = update_recovery::live_chat_descriptors(&state);
                record.verify_live_inventory(state.proof(), &terminals, &chats)
            }
            InstalledRestoration::Ready {
                admission,
                restored,
            } => {
                let chats = chat::chat_list_inventory(&state);
                if chats.iter().any(|session| {
                    state.chats.get(&session.id).is_none_or(|record| {
                        record.handle.is_completed() || !record.handle.owns_work()
                    })
                }) {
                    return Err("An original conversation has not finished resuming".into());
                }
                admission.verify_live_inventory(
                    state.proof(),
                    &terminals,
                    &chats,
                    &Default::default(),
                )?;
                let digest = restored.checkpoint_digest_for_removal();
                if admission.is_held_return() {
                    admission.finish_held_return(
                        digest.ok_or("The held return did not restore the exact checkpoint")?,
                    )
                } else {
                    admission.remember_cutover(state.proof(), digest, false)
                }
            }
        }?;
        if let Some(publication) = publication.publication {
            publication.publish().map_err(|error| error.to_string())?;
        }
        if let InstalledRestoration::Ready {
            admission,
            mut restored,
        } = recovered
        {
            if admission.is_held_return() {
                restored.finish_archived_locked(&mut state);
            } else {
                restored.finish_locked(&mut state)?;
            }
        }
        Ok::<_, String>(())
    })();
    if let Err(error) = result {
        let detail = format!("Could not select the restored application: {error}");
        state.retain_startup_recovery(detail.clone());
        tracing::error!(%detail, "restored startup owners were kept");
    }
    state.publishing_startup = false;
    state.recheck_history_readers();
}

#[cfg(target_os = "macos")]
fn select_parked_terminals(
    host: &SharedHost,
    admission: &crate::upgrade::recovery::ReadyAdmission,
    publication: crate::update::LocalPublication,
    checkpoint: &crate::upgrade::handoff::Checkpoint,
) -> Result<(), String> {
    let (terminals, proof) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (state.parked_update_inventory()?, state.proof().clone())
    };
    admission.verify_parked_terminals(&terminals)?;
    if !state::parked_native_roster_matches(checkpoint, &proof)? {
        return Err("An original retained conversation lost custody before selection".into());
    }
    if daemon::termination_requested() {
        return Err("The daemon was stopped before selecting the installed update".into());
    }
    admission.remember_cutover(admission.successor(), None, true)?;
    if daemon::termination_requested() {
        return Err("The daemon was stopped before selecting the installed update".into());
    }
    publication.publish().map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
fn activate_parked_terminals(
    host: &SharedHost,
    admission: Option<&crate::upgrade::recovery::ReadyAdmission>,
    mut installed: update_recovery::ParkedInstalledReady,
    mut serve_status: impl FnMut(std::time::Instant) -> io::Result<()>,
) -> Result<(), String> {
    if let Some(admission) = admission {
        admission
            .begin_workspace_restore()
            .map_err(|error| error.to_string())?;
    }
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .grant_parked_update()?;
    let wait_budget = std::time::Duration::from_secs(10);
    let mut await_receipt = |deadline| {
        if daemon::termination_requested() || std::time::Instant::now() >= deadline {
            return Err("Original terminal promotion or final screen remains unconfirmed".into());
        }
        // Promotion and completed-screen receipts wake the daemon.
        serve_status(deadline).map_err(|error| error.to_string())
    };
    let deadline = std::time::Instant::now() + wait_budget;
    loop {
        let settled = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.parked_update_promotions_confirmed()?
                && state.parked_terminal_results()?.is_some()
        };
        if settled {
            break;
        }
        await_receipt(deadline)?;
    }
    update_recovery::finish_parked_ready(host, installed.capture, &installed.checkpoint)?;
    save_abandoned_identities(host);
    workspace::adopt_foreign_panes(host);
    save_agent_pane_identities(host);
    let deadline = std::time::Instant::now() + wait_budget;
    let (mut state, terminals, completed) = loop {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let terminals = state.list(0);
        if let Some(completed) = state.parked_terminal_results()? {
            break (state, terminals, completed);
        }
        drop(state);
        await_receipt(deadline)?;
    };
    let chats = chat::chat_list_inventory(&state);
    if chats.iter().any(|session| {
        state
            .chats
            .get(&session.id)
            .is_none_or(|record| record.handle.is_completed() || !record.handle.owns_work())
    }) {
        return Err("An original retained conversation is not ready".into());
    }
    if let Some(admission) = admission {
        admission.verify_live_inventory(state.proof(), &terminals, &chats, &completed)?;
    } else {
        crate::update::verify_selected_parked_inventory(
            state.proof(),
            &terminals,
            &chats,
            &completed,
        )
        .map_err(|error| error.to_string())?;
    }
    installed.restored.finish_locked(&mut state)
}

fn acknowledge_standby(standby: Option<daemon::standby::Activated>) {
    if let Some(standby) = standby
        && let Err(error) = standby.started()
    {
        // The host owns the restored state even after the coordinator exits.
        tracing::warn!(%error, "replacement daemon startup acknowledgement was not delivered");
    }
}

impl DaemonAdmission {
    #[doc(hidden)]
    pub fn with_update_standby(mut self, standby: Option<daemon::standby::Activated>) -> Self {
        self.standby = standby;
        self
    }
}

/// Authenticate startup authority before diagnostics or runtime threads exist.
///
/// # Safety
/// Call from the single-threaded entry point: this clears inherited authority
/// markers so later terminal children cannot inherit a consumed descriptor.
pub unsafe fn prepare_daemon_admission(
    #[cfg(target_os = "macos")] standby: Option<&daemon::standby::Activated>,
) -> Result<DaemonAdmission, HostError> {
    use crate::instance::DaemonAdmissionGuard;
    #[cfg(target_os = "macos")]
    let admission = standby.map_or_else(
        DaemonAdmissionGuard::acquire,
        DaemonAdmissionGuard::acquire_for_standby,
    )?;
    #[cfg(not(target_os = "macos"))]
    let admission = DaemonAdmissionGuard::acquire()?;
    // The authenticated inherited launch fence belongs to the updater until
    // it has reopened and verified every saved window. A plain cold start owns
    // its own post-Ready completion instead.
    let retirement_owned_by_updater = std::env::var_os(crate::instance::INSTALL_GUARD_ENV)
        .is_some()
        || crate::upgrade::recovery::has_inherited_receipt()?;
    // SAFETY: the caller guarantees that no other thread accesses environment.
    unsafe {
        std::env::remove_var(crate::instance::DAEMON_FENCE_ENV);
        std::env::remove_var(crate::instance::INSTALL_GUARD_ENV);
        crate::instance::take_daemon_at_boot_marker();
    }
    // SAFETY: this is still the single-threaded entry point.
    let lease = unsafe { daemon::lease_handoff::take_inherited_lease()? };
    Ok(DaemonAdmission {
        _guard: admission,
        lease,
        retirement_owned_by_updater,
        standby: None,
    })
}

/// How often the daemon reclaims runtime-directory space that its chats no
/// longer need. The directory is RAM-backed, so leaving this to the next
/// daemon start lets a long-running daemon hold it for days.
const RUNTIME_COLLECT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// How often the daemon stops session scopes that are empty but still loaded.
const SCOPE_COLLECT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// An empty session scope stays loaded in the user manager until it exits.
/// Enrollment withdraws its own, but a manager too slow to answer can outlast
/// that, and earlier versions left them too. Collecting them waits on the
/// manager, so it runs beside the daemon, for as long as the process lives.
fn start_scope_collection() -> io::Result<()> {
    service::periodic("cagents-scope-collect", SCOPE_COLLECT_INTERVAL, || {
        match crate::daemon::process_scope::collect_empty_scopes() {
            Ok(0) => {}
            Ok(stopped) => tracing::info!(stopped, "stopped empty session scopes"),
            Err(error) => tracing::warn!(%error, "empty session scopes were not collected"),
        }
        service::Next::Continue
    })
    .map(drop)
}

/// Runs background storage collection until the daemon is gone.
fn start_runtime_collection(
    host: std::sync::Weak<state::HostLock>,
    root: std::path::PathBuf,
    model: Option<model::Registry>,
) -> io::Result<()> {
    let mut next_upgrade_collection = std::time::Instant::now();
    runtime_collector(host, RUNTIME_COLLECT_INTERVAL, move |host, first| {
        let now = std::time::Instant::now();
        if now >= next_upgrade_collection {
            next_upgrade_collection = now + std::time::Duration::from_secs(24 * 60 * 60);
            let serving = host
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .proof()
                .clone();
            match service::attempt("upgrade evidence collection", || {
                crate::upgrade::recovery::collect_superseded(&serving)
            }) {
                Ok(0) => {}
                Ok(files) => tracing::info!(files, "removed expired upgrade evidence"),
                Err(error) => tracing::warn!(%error, "upgrade evidence collection did not finish"),
            }
        }
        collect_runtime_directory(host, &root, model.as_ref(), first)
    })
    .map(drop)
}

/// Runs `pass` with the host every `interval` until the host is gone. Only the
/// first pass is told it is first. A panicking pass is retried.
fn runtime_collector(
    host: std::sync::Weak<state::HostLock>,
    interval: std::time::Duration,
    mut pass: impl FnMut(SharedHost, bool) + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    let mut first = true;
    service::periodic("cagents-runtime-collect", interval, move || {
        let Some(host) = host.upgrade() else {
            return service::Next::Stop;
        };
        pass(host, std::mem::replace(&mut first, false));
        service::Next::Continue
    })
}

/// One collection pass. Each chore is contained on its own, so one that keeps
/// panicking does not keep the others from running.
fn collect_runtime_directory(
    host: SharedHost,
    root: &std::path::Path,
    model: Option<&model::Registry>,
    first: bool,
) {
    if !first {
        match service::attempt("transcript cache cleanup", || {
            crate::transcript_cache::sweep(root).map_err(|error| error.to_string())
        }) {
            Ok(usage) if usage.count > 0 => tracing::info!(
                caches = usage.count,
                bytes = usage.bytes,
                "reclaimed transcript caches"
            ),
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "transcript cache cleanup was refused; evidence preserved")
            }
        }
    }
    match service::attempt("retired actor receipt compaction", || {
        chat::retained_actor::compact_retired_receipts(&host)
    }) {
        Ok(0) => {}
        Ok(bytes) => tracing::info!(bytes, "released retired actors' final transcripts"),
        Err(error) => tracing::warn!(%error, "retired actor receipts were not compacted"),
    }
    // Inventory reads use their own connection. Only final admission takes
    // the workspace writer and host locks.
    if let Some(model) = model {
        match service::attempt("closed-session notice collection", || {
            notifications::orphaned::collect(&host, model)
        }) {
            Ok(0) => {}
            Ok(notices) => tracing::debug!(notices, "acknowledged attention for closed sessions"),
            Err(error) => {
                tracing::debug!(%error, "closed-session attention inventory is incomplete")
            }
        }
        match service::attempt("pane change collection", || model.collect_view_operations()) {
            Ok(0) => {}
            Ok(rows) => tracing::info!(rows, "forgot pane changes applied over a week ago"),
            Err(error) => tracing::warn!(%error, "applied pane changes were not collected"),
        }
        match service::attempt("worktree view compaction", || {
            model.compact_worktree_views()
        }) {
            Ok(Some((sequence, effects))) => {
                announce_compacted_views(&host, sequence, &effects);
                tracing::info!(
                    rows = effects.views.len(),
                    "compacted empty views of missing worktrees"
                );
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "worktree view compaction did not finish"),
        }
    }
}

fn announce_compacted_views(
    host: &SharedHost,
    sequence: u64,
    effects: &crate::workspace_model::delta::EditEffects,
) {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .announce_model_edit(
            agent_execution::EXECUTION_CLIENT,
            sequence,
            &crate::workspace_model::ModelEdit::AgentsChanged,
            effects,
        );
}

/// Reserve and configure the serving socket before restore can take ownership
/// of an original holder. The caller still holds HostLease and admission, and
/// does not accept clients or publish readiness until restoration finishes.
fn listener_before_restore(
    paths: &RuntimePaths,
    reserved: Option<UnixListener>,
    restore: impl FnOnce() -> Result<(), HostError>,
) -> Result<UnixListener, HostError> {
    let listener = match reserved {
        Some(listener) => listener,
        None => reserve_serving_listener(paths)?,
    };
    restore()?;
    Ok(listener)
}

fn reserve_serving_listener(paths: &RuntimePaths) -> io::Result<UnixListener> {
    let listener = crate::descriptor::bind(&paths.socket)?;
    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Runs `work` on this installation's shared-model store while holding the
/// daemon lease, so no daemon can open or convert the store meanwhile.
fn with_store_stopped<T>(work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let paths = RuntimePaths::discover().map_err(|error| error.to_string())?;
    let inherited =
        daemon::lease_handoff::clone_inherited_lease().map_err(|error| error.to_string())?;
    #[cfg(target_os = "macos")]
    let _publication = if inherited.is_none() {
        crate::install_guard::admit_darwin_state_access().map_err(|error| error.to_string())?
    } else {
        Some(
            crate::install_guard::admit_darwin_inherited_state_access()
                .map_err(|error| error.to_string())?,
        )
    };
    let _lease =
        HostLease::acquire_for_store_maintenance(&paths, inherited).map_err(
            |error| match error {
                PlatformError::AlreadyRunning { .. } => {
                    "a cagents daemon is running; stop it before reading or converting its store"
                        .to_owned()
                }
                error => error.to_string(),
            },
        )?;
    // A daemon in another runtime directory must not open the store while
    // this converts it. Released before the lease.
    let _state = crate::instance::state_claim::claim_store_maintenance(&paths.lock)
        .map_err(|error| error.to_string())?;
    work()
}

/// The schema of this installation's shared-model store, or `None` if there is
/// no store yet. The installer records it before a replacement daemon starts,
/// since that daemon may convert the store forward, so that a rollback can
/// convert it back without running the previous release.
pub fn stored_schema() -> Result<Option<u32>, String> {
    with_store_stopped(model::Registry::stored_schema)
}

/// Converts this installation's shared-model store back to schema `target`, so
/// a daemon that predates the current schema can open it after a rollback.
/// Refuses while a daemon runs. Returns the schema it converted from, or
/// `None` if there was nothing to do.
pub fn downgrade_store(target: u32) -> Result<Option<u32>, String> {
    with_store_stopped(|| model::Registry::downgrade(target))
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn observe_fixture_startup(phase: &'static str, error: Option<&str>) {
    crate::fixture_startup::observe(phase, error);
}

/// Saves the conversation IDs an abandoned update's checkpoint alone records,
/// so the chats they name resume their conversations rather than start new
/// ones. The archive stays as evidence; a later start finds nothing to save.
fn save_abandoned_identities(host: &SharedHost) {
    // A daemon without its shared model has nothing to save them to.
    let Ok(registry) = agent_execution::registry(host) else {
        return;
    };
    let store = match crate::upgrade::handoff::Store::existing() {
        Ok(Some(store)) => store,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, "cannot read abandoned update evidence");
            return;
        }
    };
    let checkpoints = match store.abandoned_checkpoints() {
        Ok(checkpoints) => checkpoints,
        Err(error) => {
            tracing::warn!(%error, "cannot list abandoned update archives");
            return;
        }
    };
    // A conversation restore reattached is its holder's to name: that
    // reattachment saves the identity it confirmed, through the lineage it
    // authenticates.
    let restored: std::collections::BTreeSet<_> = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .keys()
        .copied()
        .collect();
    for checkpoint in checkpoints {
        let checkpoint = match checkpoint {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                tracing::warn!(%error, "cannot read an abandoned update's conversation IDs");
                continue;
            }
        };
        let unheld: Vec<_> = checkpoint
            .chats
            .into_iter()
            .filter(|chat| !restored.contains(&chat.source.id))
            .collect();
        let saved = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .save_checkpoint_identities(&unheld);
        if !saved.written.is_empty() {
            tracing::info!(chats = ?saved.written, "saved conversation IDs only the abandoned update recorded");
        }
        for chat in saved.unrecoverable {
            let conversation = unheld
                .iter()
                .find(|entry| entry.source.id == chat)
                .and_then(|entry| entry.launch.session_id.as_deref());
            tracing::warn!(
                %chat,
                conversation,
                "the chat's pane or agent now shows another process; resume its conversation manually"
            );
        }
        for (chat, error) in saved.failed {
            tracing::warn!(%chat, %error, "cannot save the abandoned update's ID for this chat; a later start tries again unless it is reopened first");
        }
    }
}

/// Saves each open agent's conversation ID to its pane where the pane shows
/// the same process without one, so a chat whose process is gone reopens its
/// conversation instead of reporting that cagents lost track of it.
fn save_agent_pane_identities(host: &SharedHost) {
    let Ok(registry) = agent_execution::registry(host) else {
        return;
    };
    let saved = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .save_agent_pane_identities();
    if let Some(error) = saved.unread {
        tracing::warn!(%error, "cannot read the agents whose panes lack their conversation ID");
    }
    if !saved.written.is_empty() {
        tracing::info!(chats = ?saved.written, "saved agents' conversation IDs to the panes that lacked them");
    }
    for (chat, error) in saved.failed {
        tracing::warn!(%chat, %error, "cannot save this agent's conversation ID to its pane; a later start tries again");
    }
}

fn open_chat_deliveries(
    require_durable_workspace: bool,
) -> io::Result<(Option<Arc<crate::chat::runtime::Deliveries>>, bool)> {
    use crate::chat::runtime::Deliveries;

    match Deliveries::open() {
        Ok(deliveries) => return Ok((Some(Arc::new(deliveries)), true)),
        Err(error) if require_durable_workspace => {
            return Err(io::Error::other(format!(
                "Cannot reopen the message delivery ledger: {error}"
            )));
        }
        Err(error) => {
            tracing::warn!(%error, "cannot open the message delivery ledger; using memory")
        }
    }
    let deliveries = match Deliveries::in_memory() {
        Ok(deliveries) => Arc::new(deliveries),
        Err(error) => {
            tracing::error!(%error, "cannot open an in-memory message delivery ledger");
            return Ok((None, false));
        }
    };
    let fallback = Arc::downgrade(&deliveries);
    let interval = std::time::Duration::from_secs(1);
    let mut backoff = service::Backoff::new(interval);
    let mut wait = backoff.fail();
    if let Err(error) = service::spawn("cagents-chat-deliveries", interval, move || {
        thread::sleep(wait);
        let Some(deliveries) = fallback.upgrade() else {
            return service::Next::Stop;
        };
        match deliveries.restore_persistence() {
            Ok(()) => {
                tracing::info!("the persistent message delivery ledger is available again");
                service::Next::Stop
            }
            Err(error) => {
                wait = backoff.fail();
                tracing::warn!(%error, retry_seconds = wait.as_secs(),
                               "the message delivery ledger is still unavailable; using memory");
                service::Next::Continue
            }
        }
    }) {
        tracing::error!(%error, "cannot schedule reopening the message delivery ledger");
    }
    Ok((Some(deliveries), false))
}

/// Initialize the serving daemon under its runtime and saved-data leases.
/// The returned services stay owned by the daemon until its main loop exits.
fn initialize_serving_host(
    host: &SharedHost,
    paths: &RuntimePaths,
    first_this_boot: bool,
    require_durable_workspace: bool,
    notification_application: crate::notifications::Application,
) -> Result<(Option<notifications::Broker>, activity::Monitor, bool), HostError> {
    // Before anything is launched, so the epoch precedes every launch.
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof_handle();
    crate::lineage_birth::open_instance_epoch(&proof.boot_id, first_this_boot);
    // Before anything is launched or adopted: sessions launched through this
    // entry by an earlier daemon now run the CLI that speaks this protocol.
    if let Err(error) = crate::agent_execution::bundled::publish_serving_cli(&paths.directory) {
        tracing::warn!(
            %error,
            "sessions will name this version's CLI directory, which a later update does not follow"
        );
    }
    #[cfg(target_os = "macos")]
    {
        // Capture the login shell's PATH before admitting native provider
        // launches. The bounded shell probe imports only PATH and is cached.
        // LaunchServices and launchd can both start with a shorter PATH.
        if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            let _ = runtime.block_on(crate::account::probe::search_path());
        }
    }
    let codex_listeners = chat::CodexListenerRoots::in_runtime_directory(&paths.directory);
    let native_chat_capacity = crate::native_chat_capacity::load()? as usize;
    let (chat_deliveries, durable_deliveries) = open_chat_deliveries(require_durable_workspace)?;
    let prompt_cache = Arc::new(agent_execution::prompt_cache::Calibration::installation());
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.codex_listeners = codex_listeners;
        state.native_chat_capacity = native_chat_capacity;
        state.chat_deliveries = chat_deliveries;
        state.prompt_cache = prompt_cache;
        state.enable_holder_recovery();
    }
    // A daemon that cannot open the shared-model store still hosts terminals.
    // The model is reported unavailable to the clients that ask for it, which is
    // what keeps a window from mistaking a broken store for an empty registry
    // and donating its state over and over.
    #[cfg(all(test, target_os = "linux"))]
    observe_fixture_startup("opening workspace model", None);
    let durable_model = match model::Registry::open() {
        Ok(mut registry) => {
            match crate::agent_execution::concurrency::Setting::configured() {
                Ok(setting) => registry.use_concurrency(setting),
                Err(error) => tracing::warn!(
                    %error,
                    "the agent concurrency limits cannot be configured; the defaults apply"
                ),
            }
            // Before any client reads the model: a window must never see a
            // worktree's panes outside its checkout's channel.
            let adopted = match registry.adopt_worktree_views() {
                Ok(adopted) => {
                    if adopted.strips > 0 {
                        tracing::info!(
                            strips = adopted.strips,
                            panes = adopted.panes,
                            channels = adopted.created_channels,
                            kept = adopted.kept,
                            failed = adopted.failed,
                            "moved worktree strips into their checkouts' channels"
                        );
                    }
                    adopted.failed == 0
                }
                Err(error) if require_durable_workspace => {
                    return Err(io::Error::other(format!(
                        "Cannot reopen saved workspace views: {error}"
                    ))
                    .into());
                }
                Err(error) => {
                    tracing::error!(%error, "worktree strips were not moved into their checkouts; they still open as strips");
                    false
                }
            };
            let writers_available = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .install_model(registry);
            if require_durable_workspace && !(adopted && writers_available) {
                return Err(
                    io::Error::other("Cannot reopen every required workspace writer").into(),
                );
            }
            adopted && writers_available
        }
        Err(error) if require_durable_workspace => {
            return Err(io::Error::other(format!(
                "Cannot reopen the shared workspace model: {error}"
            ))
            .into());
        }
        Err(error) => {
            tracing::error!(%error, "the shared workspace model is unavailable; channels will not be shared between windows");
            false
        }
    };
    workspace::titles::install(host);
    #[cfg(all(test, target_os = "linux"))]
    observe_fixture_startup("workspace model opened", None);
    let notification_broker = match notifications::Broker::start(host, notification_application) {
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
    if let Err(error) = daemon::codex_temp::sweep() {
        tracing::warn!(%error, "stale Codex temporary directories could not be swept");
    }
    if let Err(error) = start_scope_collection() {
        tracing::warn!(%error, "empty session scopes were not collected");
    }
    // This mandatory thread service must be available before any adoption
    // can discard the holder's replay copy. It observes no process until the
    // same HostState publishes one; it initiates no execution.
    #[cfg(all(test, target_os = "linux"))]
    observe_fixture_startup("starting activity monitor", None);
    let agent_activity = activity::Monitor::start(host)?;
    #[cfg(all(test, target_os = "linux"))]
    observe_fixture_startup("activity monitor started", None);
    Ok((
        notification_broker,
        agent_activity,
        durable_deliveries && durable_model,
    ))
}

pub fn run(mut admission: DaemonAdmission) -> Result<(), HostError> {
    #[cfg(all(test, target_os = "linux"))]
    observe_fixture_startup("host run entered", None);
    let executable = crate::process::current_executable()?;
    let editor_requested =
        std::env::args_os().any(|arg| arg == crate::instance::CEDITOR_NOTIFICATION_ARGUMENT);
    // Admission holds the launch fence through startup.
    if editor_requested {
        crate::instance::validate_editor_daemon_version(&executable)?;
    }
    let notification_application =
        crate::instance::notification_daemon_application(&executable, editor_requested)?;
    daemon::install_signal_handlers()?;
    crate::descriptor_limit::raise();
    // Holders start from this image; pin it before an update replaces the
    // installed daemon.
    if let Err(error) = crate::process::pin_own_image() {
        tracing::warn!(%error, "terminal and chat holders cannot be started");
    }
    let paths = RuntimePaths::discover()?;
    let retirement_owned_by_updater = admission.retirement_owned_by_updater;
    let mut standby = admission.standby.take();
    #[cfg(target_os = "macos")]
    let mut ready_admission = standby
        .as_mut()
        .and_then(daemon::standby::Activated::take_ready_admission);
    #[cfg(target_os = "macos")]
    if ready_admission.is_none() {
        crate::update::admit_darwin_saved_state(&crate::process::current_executable()?)
            .map_err(io::Error::other)?;
    }
    #[cfg(target_os = "macos")]
    let proof = match &ready_admission {
        Some(ready) => ready.successor().clone(),
        None => daemon::new_host_session_proof()?,
    };
    #[cfg(not(target_os = "macos"))]
    let proof = daemon::new_host_session_proof()?;
    // The lock outlives every daemon but not the runtime directory, which a
    // reboot removes: without it no daemon of this instance ran this boot.
    let first_this_boot = fs::symlink_metadata(&paths.lock)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
    let acquire_runtime = |inherited| {
        #[cfg(target_os = "macos")]
        if let Some(ready) = &ready_admission {
            return ready.acquire_runtime(&paths, inherited);
        }
        HostLease::acquire_with_handoff(&paths, &proof, inherited)
    };
    #[cfg(target_os = "linux")]
    let inherited_update = admission.lease.is_some() && admission._guard.inherited_launch_fence();
    let _lease = acquire_runtime(admission.lease.take())?;
    // The lease excludes daemons only in this runtime directory. Before this
    // daemon restores, publishes or saves anything, it also claims the saved
    // data, which a daemon in another runtime directory may already serve.
    // Declared after the lease, it is released first. Darwin's installer
    // checks the production claim before changing or removing any saved data.
    let claim_state = || {
        crate::instance::state_claim::claim_daemon_state(
            &paths.lock,
            proof.host_session_id.as_str(),
        )
    };
    #[cfg(target_os = "macos")]
    let state = ready_admission
        .as_ref()
        .map_or_else(claim_state, |ready| ready.claim_saved_state(&paths.lock));
    #[cfg(not(target_os = "macos"))]
    let state = claim_state();
    let saved_state_claim = match state {
        Ok(claim) => Some(claim),
        Err(error) => match crate::instance::state_claim::state_in_use(&error) {
            Some(refusal) => return Err(HostError::StateInUse(refusal.clone())),
            #[cfg(target_os = "macos")]
            None if ready_admission.is_some()
                || !crate::instance::IS_DEVELOPMENT
                || crate::update::runs_from_installation() =>
            {
                return Err(error.into());
            }
            None => {
                tracing::warn!(%error, "could not claim this instance's saved data; a daemon in another runtime directory is not kept away from it");
                None
            }
        },
    };
    #[cfg(all(test, target_os = "linux"))]
    observe_fixture_startup("daemon lease acquired", None);
    #[cfg(target_os = "linux")]
    let accepted_ready = if inherited_update {
        match crate::update::accept_linux_update_handoff(&proof) {
            Ok(ready) => ready,
            Err(error) => {
                tracing::warn!(%error, "could not accept completion of the interrupted update");
                None
            }
        }
    } else {
        None
    };
    // A close outcome belongs to the saved window even across a daemon
    // restart. Only the daemon that holds the saved-state claim may write it.
    let receipt_root = saved_state_claim
        .as_ref()
        .and_then(|_| crate::instance::project_dirs())
        .map_or_else(
            || paths.directory.clone(),
            |dirs| dirs.data_dir().to_path_buf(),
        );
    let mut state = HostState::new(proof);
    state.background_chat_closes = Some(Arc::new(chat::background_close::Journal::new(
        receipt_root.join("chat-close-receipts"),
    )));
    let host: SharedHost = Arc::new(crate::host::state::HostLock::new(state));
    #[cfg(target_os = "macos")]
    let mut local_publication = standby
        .as_mut()
        .and_then(daemon::standby::Activated::take_local_publication);
    // Inherited Ready custody must reserve its listener before selecting a
    // staged build or claiming permission to change the workspace.
    #[cfg(target_os = "macos")]
    let reserved_listener = ready_admission
        .as_ref()
        .map(|_| reserve_serving_listener(&paths))
        .transpose()?;
    #[cfg(not(target_os = "macos"))]
    let reserved_listener = None;
    #[cfg(target_os = "macos")]
    let mut parked_installed = if let Some(ready) = &ready_admission
        && local_publication.is_some()
        && let Some(parked) =
            update_recovery::park_installed_ready(&host, ready).map_err(io::Error::other)?
    {
        select_parked_terminals(
            &host,
            ready,
            local_publication.take().unwrap(),
            &parked.checkpoint,
        )
        .map_err(io::Error::other)?;
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .publishing_startup = true;
        Some(parked)
    } else {
        None
    };
    #[cfg(target_os = "macos")]
    let mut cutover_checkpoint = {
        if standby.is_none()
            && let Err(error) = crate::update::retire_completed_cutover_checkpoint()
        {
            tracing::warn!(%error, "could not retire the selected daemon's verified checkpoint");
        }
        Some(if parked_installed.is_some() {
            crate::update::CutoverStartupCheckpoint::Unsettled
        } else {
            crate::update::cutover_startup_checkpoint().map_err(io::Error::other)?
        })
    };
    #[cfg(target_os = "macos")]
    if let Some(crate::update::CutoverStartupCheckpoint::ParkedReady { digest, .. }) =
        &cutover_checkpoint
    {
        let parked = update_recovery::park_carried_ready(&host, digest)
            .map_err(io::Error::other)?
            .ok_or_else(|| {
                io::Error::other("The selected checkpoint lost its exact terminal roster")
            })?;
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .publishing_startup = true;
        parked_installed = Some(parked);
    }
    #[cfg(target_os = "macos")]
    let require_durable_workspace = match &ready_admission {
        Some(ready) => {
            ready.begin_workspace_effects()?;
            ready.begin_workspace_initialization()?;
            ready.requires_durable_workspace() || parked_installed.is_some()
        }
        None => matches!(
            cutover_checkpoint,
            Some(crate::update::CutoverStartupCheckpoint::ParkedReady { .. })
        ),
    };
    #[cfg(not(target_os = "macos"))]
    let require_durable_workspace = false;
    let (_notification_broker, _agent_activity, _durable_workspace) = initialize_serving_host(
        &host,
        &paths,
        first_this_boot,
        require_durable_workspace,
        notification_application,
    )?;
    #[cfg(target_os = "macos")]
    if _durable_workspace && let Some(ready) = &ready_admission {
        ready.finish_workspace_initialization()?;
    }
    let mut recovered_prepared = false;
    #[cfg(target_os = "macos")]
    let mut installed_restoration = None;
    let listener = listener_before_restore(&paths, reserved_listener, || {
        #[cfg(target_os = "macos")]
        if parked_installed.is_some() {
            return Ok(());
        }
        #[cfg(all(test, target_os = "linux"))]
        observe_fixture_startup("restore inherited recovery", None);
        let restored = (|| -> io::Result<()> {
            #[cfg(target_os = "macos")]
            if let Some(admission) = ready_admission.take() {
                admission.begin_workspace_restore()?;
                let restored = update_recovery::restore_installed_ready(&host, &admission)
                    .map_err(io::Error::other)?;
                installed_restoration = Some(InstalledRestoration::Ready {
                    admission,
                    restored,
                });
                return Ok(());
            }
            if let Some(_record) = update_recovery::restore(&host).map_err(io::Error::other)? {
                recovered_prepared = true;
                #[cfg(target_os = "macos")]
                {
                    installed_restoration = Some(InstalledRestoration::Prepared(_record));
                }
                return Ok(());
            }
            #[cfg(all(test, target_os = "linux"))]
            observe_fixture_startup("restore settled Prepared", None);
            if update_recovery::restore_settled(&host).map_err(io::Error::other)? {
                recovered_prepared = true;
                return Ok(());
            }
            #[cfg(all(test, target_os = "linux"))]
            observe_fixture_startup("restore ordinary", None);
            #[cfg(target_os = "macos")]
            let ordinary = cutover_checkpoint
                .as_ref()
                .and_then(|entry| entry.saved_digest())
                .map_or_else(
                    || update_recovery::restore_ordinary(&host),
                    |digest| update_recovery::restore_observed_adoption(&host, digest),
                );
            #[cfg(target_os = "linux")]
            let ordinary = accepted_ready.as_deref().map_or_else(
                || update_recovery::restore_ordinary(&host),
                |digest| update_recovery::restore_observed_adoption(&host, digest),
            );
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let ordinary = update_recovery::restore_ordinary(&host);
            ordinary.map_err(io::Error::other)?;
            #[cfg(target_os = "macos")]
            if matches!(
                cutover_checkpoint.as_ref(),
                Some(crate::update::CutoverStartupCheckpoint::Absent)
            ) {
                update_recovery::reconnect_selected_codex(&host).map_err(io::Error::other)?;
            }
            Ok(())
        })();
        // After restore has reattached every holder it could, and before
        // the listener accepts a client that would restore a chat afresh. A
        // failed restore may not have reached a holder yet; a later start
        // saves them instead.
        if restored.is_ok() {
            save_abandoned_identities(&host);
            // A second daemon over the same data may have stamped panes of
            // chats this one serves with its own proof (#4118).
            workspace::adopt_foreign_panes(&host);
        }
        // An open agent's own ID is the daemon's record whatever restore
        // reached, so its pane learns it before any client reads the pane.
        save_agent_pane_identities(&host);
        match restored {
            Ok(()) => Ok(()),
            Err(error) => {
                #[cfg(all(test, target_os = "linux"))]
                observe_fixture_startup("restore error before cleanup", Some(&error.to_string()));
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
    let next_client_id = AtomicU64::new(1);
    let client_capacity = ClientCapacity::new(MAX_ACTIVE_CLIENTS);
    let mut clients = Vec::new();
    #[cfg(target_os = "macos")]
    if let Some(parked) = parked_installed {
        let ready = ready_admission.take();
        let activated = activate_parked_terminals(&host, ready.as_ref(), parked, |deadline| {
            reap_finished(&mut clients);
            match accept_client(
                &listener,
                &host,
                &next_client_id,
                &client_capacity,
                &mut clients,
            ) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    daemon::wait_for_activity_until(&listener, Some(deadline), &[])
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(()),
                result => result,
            }
        });
        let confirmed = activated.is_ok();
        {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(error) = activated {
                let detail = format!("Could not finish activating the selected terminals: {error}");
                state.retain_startup_recovery(detail.clone());
                tracing::error!(%detail, "retained original terminal custody and checkpoint");
            }
            state.end_parked_update();
            state.publishing_startup = false;
        }
        if confirmed {
            update_recovery::publish_archived_proof(&host);
        }
    }
    #[cfg(all(test, target_os = "linux"))]
    observe_fixture_startup("restore complete; release admission", None);
    #[cfg(target_os = "linux")]
    let local_publication = standby
        .as_mut()
        .and_then(daemon::standby::Activated::take_local_publication);
    #[cfg(target_os = "macos")]
    let returning = installed_restoration.as_ref().is_some_and(|restoration| {
        matches!(restoration, InstalledRestoration::Ready { admission, .. } if admission.is_held_return())
    });
    #[cfg(target_os = "linux")]
    let returning = false;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let mut pending_publication = if local_publication.is_some() || returning {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .publishing_startup = true;
        Some(StartupPublication {
            publication: local_publication,
            _admission: admission,
            #[cfg(target_os = "macos")]
            recovered: installed_restoration,
        })
    } else {
        drop(admission);
        None
    };
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    publish_restored_selection(&host, &mut pending_publication);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    drop(admission);
    #[cfg(all(test, target_os = "linux"))]
    observe_fixture_startup("admission released", None);
    crate::diagnostics::record(crate::diagnostics::Event::Ready);
    // Only once this daemon holds its lease and serves: a start that failed
    // must not leave a later one from a login thinking it started at boot.
    match crate::instance::record_daemon_started_at_boot(&paths.directory) {
        Ok(true) => tracing::info!(
            "started at boot; terminals take the session's variables from the user manager"
        ),
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(%error, "could not record that this daemon started at boot; a daemon replacing it will keep its own environment")
        }
    }
    // After restore, so the first pass covers every conversation it carried.
    if let Err(error) = chat::keep_legacy_codex_listeners_fresh(&host) {
        tracing::warn!(%error, "Codex listeners in the temporary directory will not be kept fresh");
    }
    let tally_host = Arc::downgrade(&host);
    let tally_updates = Arc::clone(&host.lock().unwrap_or_else(|p| p.into_inner()).tally_updates);
    let _tray = crate::tray::start(tally_updates, move || {
        let host = tally_host.upgrade()?;
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        Some(state.agent_tally())
    });

    let recovery_only = recovered_prepared
        || host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .startup_recovery()
            .is_some();
    if !recovery_only
        && let Err(error) = account_lifecycle::restore_unregistered_homes(Arc::downgrade(&host))
    {
        tracing::warn!(%error, "lost accounts will not be restored automatically");
    }
    if !recovery_only {
        let collect_host = Arc::downgrade(&host);
        let runtime_root = paths.directory.clone();
        let collect_model = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .independent_model()
            .and_then(|connection| match connection {
                Ok(connection) => Some(connection),
                Err(error) => {
                    tracing::warn!(%error, "workspace view maintenance will not run periodically");
                    None
                }
            });
        if let Err(error) = start_runtime_collection(collect_host, runtime_root, collect_model) {
            tracing::warn!(%error, "runtime directory caches will be reclaimed only at startup");
        }
    }
    #[cfg(all(test, target_os = "linux"))]
    observe_fixture_startup("starting automation broker", None);
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
    #[cfg(all(test, target_os = "linux"))]
    observe_fixture_startup("starting Work board", None);
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
    #[cfg(all(test, target_os = "linux"))]
    observe_fixture_startup("starting mailbox courier", None);
    let mut courier = if recovery_only {
        None
    } else {
        match agent_execution::mailbox::Courier::start_with_update_fence(&host) {
            Ok(courier) => Some(courier),
            Err(error) => {
                tracing::warn!(%error, "messages between sessions are unavailable");
                None
            }
        }
    };

    // The first daemon of a boot, if the person opted in, resumes what an
    // earlier boot ran beside the accept loop. A recovery-only daemon stays
    // so for its life and leaves this to the next daemon of the boot.
    if !recovery_only {
        let resume_host = Arc::downgrade(&host);
        if let Err(error) = std::thread::Builder::new()
            .name("cagents-resume-after-boot".into())
            .spawn(move || agent_execution::resume_after_boot(resume_host))
        {
            tracing::warn!(%error, "sessions will not be resumed now that this computer started");
        }
    }
    #[cfg(all(test, target_os = "linux"))]
    observe_fixture_startup("startup services started", None);
    if let Err(error) = chat::history_retirement::start(&host) {
        tracing::warn!(%error, "Automatic history reader retirement is unavailable");
    }
    // Restoring a checkpoint above may hold the host for as long as its
    // inventory takes; from here on a hold that long is a deadlock.
    let mut stall_watch = match host.watch(lock::STALL_LIMIT) {
        Ok(watch) => Some(watch),
        Err(error) => {
            tracing::warn!(%error, "a wedged host mutex will not be detected");
            None
        }
    };

    let mut runtime_error = None;
    let mut ordinary_exit_drain: Option<crate::daemon::child_reaper::ExitDrain> = None;
    let mut ordinary_cleanup_started = false;
    let mut sealed_deliveries = None;
    let mut lifecycle = lifecycle::Lifecycle::new();
    tracing::info!(
        instance = crate::instance::NAME,
        socket = %paths.socket.display(),
        "cagents daemon is ready"
    );

    #[cfg(all(test, target_os = "linux"))]
    exit_drain_tests::observe_ready(&host);
    if !retirement_owned_by_updater {
        // This thread must use the actual socket after startup, never a
        // synchronous self-query in listener_before_restore. Its connection
        // is served by the accept loop immediately below.
        if let Err(error) = std::thread::Builder::new()
            .name("cagents-recovery-complete".into())
            .spawn(|| {
                if let Err(error) = crate::update::recovery::refresh_completed_service() {
                    // Durable completion is separate from current daemon
                    // readiness. Keep the serving owners and listener usable.
                    tracing::warn!(%error, "recovery completion proof is unavailable; serving owners were kept");
                }
            })
        {
            tracing::warn!(%error, "recovery completion proof could not be scheduled; serving owners were kept");
        }
    }
    #[cfg(target_os = "linux")]
    acknowledge_standby(standby.take());
    // The settlement thread authenticates the serving socket. Start it only
    // when synchronous startup work is done and the accept loop can answer.
    #[cfg(not(target_os = "macos"))]
    crate::update::settle_as_successor(
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .proof()
            .clone(),
    );
    loop {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        publish_restored_selection(&host, &mut pending_publication);
        #[cfg(target_os = "macos")]
        if pending_publication.is_none() {
            acknowledge_standby(standby.take());
            if let Some(cutover_checkpoint) = cutover_checkpoint.take() {
                let serving = host
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .proof()
                    .clone();
                let host = Arc::clone(&host);
                crate::update::settle_as_successor(serving, cutover_checkpoint, move || {
                    let state = host.lock().unwrap_or_else(|p| p.into_inner());
                    let mut chats = chat::chat_list_inventory(&state);
                    chats.retain(|session| {
                        state.chats.get(&session.id).is_some_and(|record| {
                            record.handle.owns_work() && !record.handle.is_completed()
                        })
                    });
                    crate::update::CutoverLiveInventory {
                        completed: state.retained_terminal_results(),
                        terminals: state.list(0),
                        chats,
                        adopted_sources: state.upgrade_chats.clone(),
                    }
                });
            }
        }
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

        let deadline =
            lifecycle.maintain(&host, client_capacity.active.load(Ordering::Acquire) != 0);
        if ordinary_exit_drain.is_none()
            && host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_shutting_down()
        {
            #[cfg(all(test, target_os = "linux"))]
            exit_drain_tests::lifecycle_retention_tests::observe_lifecycle_idle_shutdown();
            // Lifecycle idle uses the same begin/seal path as requested exit.
            // A pending drain must keep accepting inspection and checked Close.
            continue;
        }
        let fatal = match accept_client(
            &listener,
            &host,
            &next_client_id,
            &client_capacity,
            &mut clients,
        ) {
            Ok(()) => None,
            // Nothing is pending. Park until a peer connects, a client thread
            // exits, a holder exits, a signal arrives, or a real retention/development
            // idle deadline expires. There is no periodic polling timer.
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                daemon::wait_for_activity_until(&listener, deadline, lifecycle.waiting_holders())
                    .err()
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
    let _control = gate.exclusive();
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
    // Every join budget counts from this one instant, so the batch, including
    // pending joins, ends within the longest budget: an unfinished owner
    // cannot consume another join budget per sibling.
    let started = std::time::Instant::now();
    for handle in handles {
        handle.request_shutdown();
    }
    handles
        .iter()
        .map(|handle| handle.shutdown_until(started + handle.shutdown_join_budget()))
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
            chat.decide_detached_work(agent_execution::detached_work_on_close(host, session.id));
            chat.close().and_then(|()| {
                if !upgrading {
                    // Closing the provider leaves its retained helper alive
                    // until final outcomes are durably committed and ACKed.
                    // Preserve that ownership on failure, before retiring the
                    // delegated session record below. A paired CLI view closes
                    // first, with the proof its resume requires.
                    chat::retained_actor::close_paired_view(host, session.id)
                        .and_then(|()| chat::retained_actor::finish_close(host, &chat, &session))
                } else {
                    Ok(())
                }
            })
        };
        match closed {
            Ok(()) if !upgrading => {
                {
                    let state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    // An already-acknowledged helper outbox can be empty at
                    // close even though its published request is still unread.
                    if state
                        .chats
                        .get(&session.id)
                        .is_some_and(|record| record.handle.same_attachment(&chat))
                        && let Some(notifications) = &state.notifications
                    {
                        notifications.clear_session_waiting(session.id);
                    }
                }
                // Ordinary shutdown proved this exact provider stopped. End its
                // delegated authority and runs before a new daemon can resume
                // the UI descriptor with another proof; absence alone cannot do
                // this. The agent, pane and channel stay so it resumes in place.
                if let Err(error) = agent_execution::stopped(host, &session) {
                    tracing::warn!(%error, "delegated provider stop was not saved");
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

    /// A panicking pass must not end collection: the next pass still runs,
    /// and the collector stops only once the host is gone.
    #[test]
    fn a_panicking_runtime_collection_pass_runs_again() {
        let host = service::test_host();
        let (passes, passed) = std::sync::mpsc::channel();
        let collector = runtime_collector(
            Arc::downgrade(&host),
            std::time::Duration::from_millis(1),
            move |_host, first| {
                passes.send(first).unwrap();
                if first {
                    panic!("first collection pass fails");
                }
            },
        )
        .unwrap();
        let prompt = std::time::Duration::from_secs(10);
        assert_eq!(passed.recv_timeout(prompt), Ok(true));
        assert_eq!(passed.recv_timeout(prompt), Ok(false));
        assert!(!collector.is_finished());
        drop(host);
        collector
            .join()
            .expect("the collector stops once the host is gone");
    }

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
            crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("session"),
                boot_id: crate::model::BootId::new("boot"),
                host_incarnation: crate::model::HostIncarnation::new("incarnation"),
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
