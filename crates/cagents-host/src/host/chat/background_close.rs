//! A pane close is accepted once its receipt exists. Provider teardown and
//! its outcome outlive the submitting socket and the window that owned it.
use super::{CloseTarget, Session, SharedHost, close_captured, close_retained_from_pane};
use crate::{
    agent_execution::{HostedSession, same_chat_process},
    model::TerminalId,
    terminal_ipc::{ChatCloseOutcome, ProtocolError, ProtocolErrorCode},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

use super::super::control_gate::OwnedShared;

const MAX_RUNNING: usize = 32;
const ACKNOWLEDGED_LIMIT: usize = 256;
// Shorter than the control client's read timeout. The worker wakes readers
// immediately, so no polling delay is added to an ordinary reopen.
const READ_WAIT: Duration = Duration::from_secs(2);
const INTERRUPTED: &str = "The daemon stopped before it recorded whether this chat closed.";

pub(in crate::host) struct Journal {
    directory: PathBuf,
    state: Mutex<Receipts>,
    changed: Condvar,
}

#[derive(Default)]
struct Receipts {
    running: BTreeMap<Uuid, TerminalId>,
    unwritten: BTreeMap<Uuid, Receipt>,
    missing: VecDeque<Uuid>,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    chat: TerminalId,
    result: Option<Result<(), String>>,
}

pub(in crate::host) fn journal(host: &SharedHost) -> Result<Arc<Journal>, ProtocolError> {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .background_chat_closes
        .clone()
        .ok_or_else(|| store_error("chat close storage is unavailable"))
}

impl Journal {
    pub(in crate::host) fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            state: Mutex::default(),
            changed: Condvar::new(),
        }
    }

    pub(in crate::host) fn begin(
        self: &Arc<Self>,
        host: &SharedHost,
        id: Uuid,
        session: Session,
        admission: OwnedShared,
        abandoned: impl FnOnce() -> bool,
    ) -> Result<(), ProtocolError> {
        if id.is_nil() {
            return Err(invalid(
                "A chat close receipt must have a nonempty identity",
            ));
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let existing = if let Some(receipt) = state.unwritten.get(&id) {
            Some(receipt.chat)
        } else {
            self.load(id)?.map(|receipt| receipt.chat)
        };
        if let Some(chat) = existing {
            return if chat == session.id {
                Ok(())
            } else {
                Err(invalid("This receipt belongs to a different chat close"))
            };
        }
        if state.missing.contains(&id) {
            return Err(invalid("A window already settled this close as unaccepted").not_admitted());
        }
        if state.running.len() + state.unwritten.len() >= MAX_RUNNING
            || state.running.values().any(|chat| *chat == session.id)
        {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                "A chat close is already in progress. Wait for it to finish.",
                true,
            ));
        }

        // Capture the admitted resident attachment before the window is told
        // it can leave. No work subsequently follows a reused session ID.
        let target = capture(host, &session);
        // The next window can read a missing receipt as soon as the old
        // socket goes away. Check that socket under the same lock as the
        // read, before creating a receipt or doing any provider work.
        if abandoned() {
            return Err(invalid(
                "The window disconnected before the daemon accepted the chat close",
            )
            .not_admitted());
        }
        let chat = session.id;
        Self::save_at(&self.directory, id, &Receipt { chat, result: None })?;
        state.running.insert(id, chat);
        self.changed.notify_all();

        let journal = Arc::clone(self);
        let host = Arc::clone(host);
        let worker = std::thread::Builder::new()
            .name("chat-pane-close".into())
            .spawn(move || {
                let _admission = admission;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    stop(&host, &session, target?)
                }))
                .unwrap_or_else(|_| Err("The daemon failed while stopping the chat.".into()));
                journal.finish(id, chat, result);
                crate::daemon::wake();
            });
        drop(state);
        if let Err(error) = worker {
            self.finish(
                id,
                chat,
                Err(format!("Could not start the chat close: {error}")),
            );
        }
        Ok(())
    }

    fn finish(&self, id: Uuid, chat: TerminalId, result: Result<(), String>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let receipt = Receipt {
            chat,
            result: Some(result),
        };
        if let Err(error) = Self::save_at(&self.directory, id, &receipt) {
            tracing::error!(%id, error = %error.message, "could not record a chat close outcome");
            state.unwritten.insert(id, receipt);
        }
        state.running.remove(&id);
        self.changed.notify_all();
    }

    pub(in crate::host) fn read(&self, id: Uuid) -> Result<ChatCloseOutcome, ProtocolError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let deadline = Instant::now() + READ_WAIT;
        loop {
            let pending = state.running.contains_key(&id);
            if !pending {
                if let Some(receipt) = state.unwritten.get(&id) {
                    let result = receipt
                        .result
                        .clone()
                        .expect("settled close has an outcome");
                    if Self::save_at(&self.directory, id, receipt).is_ok() {
                        state.unwritten.remove(&id);
                    }
                    return Ok(ChatCloseOutcome::Finished(result));
                }
                if let Some(receipt) = self.load(id)? {
                    let result = receipt.result.unwrap_or_else(|| Err(INTERRUPTED.into()));
                    return Ok(ChatCloseOutcome::Finished(result));
                }
                if state.missing.contains(&id) {
                    return Ok(ChatCloseOutcome::Missing);
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if pending {
                    return Ok(ChatCloseOutcome::Pending);
                }
                // A named window can read its copied receipt while the source
                // window is handing it off. Give that handoff time to arrive,
                // then settle the absence under the same lock as Begin.
                if state.missing.len() == ACKNOWLEDGED_LIMIT {
                    state.missing.pop_front();
                }
                state.missing.push_back(id);
                return Ok(ChatCloseOutcome::Missing);
            }
            (state, _) = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|p| p.into_inner());
        }
    }

    pub(in crate::host) fn acknowledge(&self, id: Uuid) -> Result<(), ProtocolError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.running.contains_key(&id) {
            return Err(invalid("The chat close is still pending"));
        }
        let stored = Self::load_at(&self.directory, id)?;
        let Some(receipt) = state.unwritten.get(&id).or(stored.as_ref()) else {
            return Ok(());
        };
        // A named window can copy another window's snapshot while that window
        // still holds the receipt. Keep a bounded recent history of consumed
        // outcomes so either copy can ask for the same answer.
        let acknowledged = self.directory.join("acknowledged");
        Self::save_at(&acknowledged, id, receipt)?;
        match fs::remove_file(self.directory.join(id.to_string())) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(store_error(error)),
        }
        state.unwritten.remove(&id);
        if let Err(error) = prune_acknowledged(&acknowledged) {
            tracing::warn!(%error, "could not trim acknowledged chat close outcomes");
        }
        Ok(())
    }

    fn load(&self, id: Uuid) -> Result<Option<Receipt>, ProtocolError> {
        if let Some(receipt) = Self::load_at(&self.directory, id)? {
            return Ok(Some(receipt));
        }
        Self::load_at(&self.directory.join("acknowledged"), id)
    }

    fn load_at(directory: &Path, id: Uuid) -> Result<Option<Receipt>, ProtocolError> {
        match fs::File::open(directory.join(id.to_string())) {
            Ok(file) => serde_json::from_reader(file).map(Some).map_err(store_error),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(store_error(error)),
        }
    }

    fn save_at(directory: &Path, id: Uuid, receipt: &Receipt) -> Result<(), ProtocolError> {
        match fs::create_dir(directory) {
            Ok(()) => {
                if let Some(parent) = directory.parent() {
                    fs::File::open(parent)
                        .and_then(|directory| directory.sync_all())
                        .map_err(store_error)?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(store_error(error)),
        }
        let mut file = tempfile::NamedTempFile::new_in(directory).map_err(store_error)?;
        serde_json::to_writer(file.as_file_mut(), receipt).map_err(store_error)?;
        file.as_file().sync_all().map_err(store_error)?;
        file.persist(directory.join(id.to_string()))
            .map_err(store_error)?;
        // The rename publishes the record to readers. A later syncing error
        // cannot turn this into a refusal with a published pending receipt.
        if let Err(error) = fs::File::open(directory).and_then(|directory| directory.sync_all()) {
            tracing::warn!(%id, %error, "could not sync the chat close receipt directory");
        }
        Ok(())
    }
}

fn prune_acknowledged(directory: &Path) -> io::Result<()> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| Uuid::parse_str(name).is_ok())
        {
            files.push((entry.metadata()?.modified()?, entry.path()));
        }
    }
    let excess = files.len().saturating_sub(ACKNOWLEDGED_LIMIT);
    if excess > 0 {
        files.sort_unstable();
        for (_, path) in files.into_iter().take(excess) {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn invalid(message: &str) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}

fn store_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::Internal,
        format!("Could not access the chat close receipt: {error}"),
        true,
    )
}

fn capture(host: &SharedHost, source: &Session) -> Result<Option<CloseTarget>, String> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.chat_spawning.contains_key(&source.id) || state.chat_handoffs.contains_key(&source.id)
    {
        return Err(
            "The conversation is changing. Wait for it to finish before closing it.".into(),
        );
    }
    let Some(record) = state.chats.get(&source.id) else {
        return Ok(None);
    };
    if record.session != *source && record.current_session() != *source {
        return Err("The hosted chat changed before close. Its replacement was kept.".into());
    }
    CloseTarget::capture(&state, record)
        .map(Some)
        .map_err(|error| error.message)
}

fn stop(host: &SharedHost, source: &Session, target: Option<CloseTarget>) -> Result<(), String> {
    let retired_source = target
        .as_ref()
        .map_or(source, |target| &target.source)
        .clone();
    let was_resident = target.is_some();
    if let Some(target) = target {
        close_captured(host, target).map_err(|error| error.message)?;
    }

    // The ordinary close ends the resident actor and its pair. A reopened
    // but unused managed chat may still have an agent bound to that stopped
    // process. Explicitly closing its pane retires that agent as well.
    let agent_remains = match super::super::agent_execution::registry(host) {
        Ok(registry) => registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .agent_for_session(source.id)?
            .is_some_and(|agent| {
                !agent.closed
                    && matches!(&agent.session, Some(HostedSession::Chat(bound))
                        if same_chat_process(bound, &retired_source))
            }),
        Err(_) => false,
    };
    let may_be_retained = source.terminal.is_some()
        || source.owned_codex_listener.is_some()
        || source.native_continuity.retains_provider();
    if agent_remains || (!was_resident && may_be_retained) {
        close_retained_from_pane(host, &retired_source)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        chat::{
            Command, Launch,
            protocol::{Adapter, ProcessSpec, Step},
            runtime::{self, Handle},
        },
        control::ControlClient,
        host::{
            chat::Record,
            client::serve_fixture_user,
            state::{ClientId, HostLock, HostState},
        },
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe},
    };
    use std::{os::unix::net::UnixStream, thread};

    fn host(root: &std::path::Path) -> (SharedHost, Session) {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("chat-close"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let mut state = HostState::new(proof.clone());
        state.background_chat_closes = Some(Arc::new(Journal::new(root.join("receipts"))));
        let source = Session {
            id: TerminalId::new(),
            native_continuity: Default::default(),
            owned_codex_listener: None,
            launch: Launch {
                provider: Provider::Omp,
                recipe: LaunchRecipe {
                    program: "/bin/cat".into(),
                    arguments: Vec::new(),
                    working_directory: root.to_path_buf(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: None,
                draft_model: None,
            },
            proof: Some(proof),
            terminal: None,
        };
        (Arc::new(HostLock::new(state)), source)
    }

    fn install(host: &SharedHost, session: Session, handle: Handle) {
        host.lock().unwrap().chats.insert(
            session.id,
            Record {
                execution_instance: Uuid::new_v4(),
                console_recipe: None,
                paired_terminal: None,
                session,
                handle,
                source_process: None,
                subscribers: Default::default(),
                observations: Default::default(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
    }

    fn connect(host: &SharedHost, id: ClientId) -> (ControlClient, thread::JoinHandle<()>) {
        let (client, server) = UnixStream::pair().unwrap();
        let host = Arc::clone(host);
        let server = thread::spawn(move || serve_fixture_user(server, id, host));
        (ControlClient::from_stream(client).unwrap(), server)
    }

    #[test]
    fn accepted_close_outlives_the_submitter_and_repeated_receipt() {
        struct Blocked {
            entered: crossbeam_channel::Sender<()>,
            release: crossbeam_channel::Receiver<()>,
        }
        impl Adapter for Blocked {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step::default())
            }

            fn command(&mut self, _: Command) -> Result<Step, String> {
                self.entered.send(()).map_err(|error| error.to_string())?;
                self.release
                    .recv_timeout(Duration::from_secs(10))
                    .map_err(|error| error.to_string())?;
                Ok(Step::default())
            }

            fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
                Ok(Step::default())
            }
        }

        let root = tempfile::tempdir().unwrap();
        let (host, source) = host(root.path());
        let spec = ProcessSpec::from(&source.launch.recipe);
        let mut initial = Step::default();
        let store = runtime::preflight(&spec, &mut initial).unwrap();
        let (entered, pending) = crossbeam_channel::bounded(1);
        let (release, released) = crossbeam_channel::bounded(1);
        let actor = Handle::start(
            spec,
            Box::new(Blocked {
                entered,
                release: released,
            }),
            initial,
            store,
            None,
            |_| {},
        )
        .unwrap();
        install(&host, source.clone(), actor.clone());
        let command = thread::spawn(move || actor.command(Command::Interrupt));
        pending.recv_timeout(Duration::from_secs(2)).unwrap();

        let receipt = Uuid::new_v4();
        let (mut first, server) = connect(&host, 1);
        first.begin_chat_close(receipt, source.clone()).unwrap();
        drop(first);
        server.join().unwrap();

        let (mut next, server) = connect(&host, 2);
        assert_eq!(
            next.read_chat_close(receipt).unwrap(),
            ChatCloseOutcome::Pending
        );
        assert!(next.acknowledge_chat_close(receipt).is_err());
        next.begin_chat_close(receipt, source.clone()).unwrap();
        assert!(host.lock().unwrap().control_gate.try_exclusive().is_none());
        release.send(()).unwrap();
        let _ = command.join().unwrap();

        assert_eq!(
            next.read_chat_close(receipt).unwrap(),
            ChatCloseOutcome::Finished(Ok(()))
        );
        assert!(!host.lock().unwrap().chats.contains_key(&source.id));
        // The original close may be retried after a lost acknowledgement.
        // A completed receipt still answers without following the new owner.
        install(&host, source.clone(), Handle::fixture_waiting_remote());
        next.begin_chat_close(receipt, source.clone()).unwrap();
        assert_eq!(
            next.read_chat_close(receipt).unwrap(),
            ChatCloseOutcome::Finished(Ok(()))
        );
        assert!(host.lock().unwrap().chats.contains_key(&source.id));
        next.acknowledge_chat_close(receipt).unwrap();
        assert_eq!(
            next.read_chat_close(receipt).unwrap(),
            ChatCloseOutcome::Finished(Ok(()))
        );
        assert!(
            !root
                .path()
                .join("receipts")
                .join(receipt.to_string())
                .exists()
        );
        drop(next);
        server.join().unwrap();
    }

    #[test]
    fn a_missing_read_and_a_disconnected_submitter_each_fence_a_late_close() {
        let root = tempfile::tempdir().unwrap();
        let (host, source) = host(root.path());
        let journal = journal(&host).unwrap();
        let receipt = Uuid::new_v4();
        let gate = Arc::clone(&host.lock().unwrap().control_gate);
        let shared = gate.shared();

        assert_eq!(journal.read(receipt).unwrap(), ChatCloseOutcome::Missing);
        assert!(
            journal
                .begin(
                    &host,
                    receipt,
                    source.clone(),
                    shared.retain(Arc::clone(&gate)),
                    || false
                )
                .is_err()
        );
        assert_eq!(journal.read(receipt).unwrap(), ChatCloseOutcome::Missing);

        let abandoned = Uuid::new_v4();
        assert!(
            journal
                .begin(
                    &host,
                    abandoned,
                    source,
                    shared.retain(Arc::clone(&gate)),
                    || true
                )
                .is_err()
        );
        assert!(journal.load(abandoned).unwrap().is_none());
    }

    #[test]
    fn a_close_does_not_report_success_while_another_client_starts_the_chat() {
        let root = tempfile::tempdir().unwrap();
        let (host, source) = host(root.path());
        host.lock()
            .unwrap()
            .chat_spawning
            .insert(source.id, source.clone());

        let receipt = Uuid::new_v4();
        let (mut client, server) = connect(&host, 1);
        client.begin_chat_close(receipt, source.clone()).unwrap();
        assert!(matches!(
            client.read_chat_close(receipt).unwrap(),
            ChatCloseOutcome::Finished(Err(reason)) if reason.contains("conversation is changing")
        ));
        assert!(host.lock().unwrap().chat_spawning.contains_key(&source.id));
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn an_exact_source_refusal_can_be_read_after_the_daemon_is_replaced() {
        let root = tempfile::tempdir().unwrap();
        let (original, mut source) = host(root.path());
        install(&original, source.clone(), Handle::fixture_waiting_remote());
        source.launch.session_id = Some("a different provider conversation".into());
        let receipt = Uuid::new_v4();
        let (mut first, server) = connect(&original, 1);
        first.begin_chat_close(receipt, source.clone()).unwrap();
        drop(first);
        server.join().unwrap();
        let expected = journal(&original).unwrap().read(receipt).unwrap();
        assert!(matches!(&expected, ChatCloseOutcome::Finished(Err(reason))
            if reason.contains("hosted chat changed")));
        assert!(original.lock().unwrap().chats.contains_key(&source.id));

        let (successor, _) = host(root.path());
        let (mut next, server) = connect(&successor, 2);
        assert_eq!(next.read_chat_close(receipt).unwrap(), expected);
        assert_eq!(next.read_chat_close(receipt).unwrap(), expected);
        next.acknowledge_chat_close(receipt).unwrap();
        next.acknowledge_chat_close(receipt).unwrap();
        assert_eq!(next.read_chat_close(receipt).unwrap(), expected);
        drop(next);
        server.join().unwrap();

        let (other, _) = host(root.path());
        assert_eq!(journal(&other).unwrap().read(receipt).unwrap(), expected);
    }
}
