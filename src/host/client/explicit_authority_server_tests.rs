//! The real framed server and real PTYs, with an issued private caller scope.
//! Fault hooks surround the real request handler; they never fabricate replies.
use super::*;
use crate::{
    agent_execution::{Authority, Lineage},
    control::ControlClient,
    daemon::{HostLease, RuntimePaths},
    host::state::{HostLock, HostState},
    model::{
        HostIncarnation, HostSessionId, LaunchRecipe, TerminalContinuity, TerminalId,
        TerminalSession,
    },
    terminal::{HostedTerminalHandle, TerminalEvent, TerminalHandle, TerminalSpawnOptions},
    terminal_ipc::TerminalCloseResult,
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs,
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, net::UnixListener},
    },
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

const FIRST: &[u8] = b"FIRST_ONCE\n";
const SECOND: &[u8] = b"SECOND_AFTER_AUTH\n";

#[derive(Clone, Debug, PartialEq, Eq)]
enum Operation {
    Authenticate,
    List,
    Spawn(TerminalId),
    Attach(TerminalId),
    Visibility(TerminalId),
    OutputAck(TerminalId),
    Input(TerminalId, Vec<u8>),
    Other,
}

#[derive(Clone, Debug)]
struct Observed {
    client: ClientId,
    operation: Operation,
    refused: Option<ProtocolErrorCode>,
}

struct Hooks {
    observed: Mutex<Vec<Observed>>,
    lose_first_reply: AtomicBool,
    lost_reply: AtomicBool,
    paused_auth: AtomicBool,
    paused_source: AtomicBool,
    source_ready: crossbeam_channel::Sender<ClientId>,
    source_release: crossbeam_channel::Receiver<()>,
    gated: crossbeam_channel::Sender<ClientId>,
    release: crossbeam_channel::Receiver<()>,
}

thread_local! {
    static CONNECTION: RefCell<Option<(Arc<Hooks>, UnixStream)>> = const { RefCell::new(None) };
}

fn observed_request(
    host: &SharedHost,
    client: ClientId,
    request: ClientRequest,
) -> Result<RequestOutcome, ProtocolError> {
    let operation = match &request {
        ClientRequest::Authenticate { .. } => Operation::Authenticate,
        ClientRequest::List => Operation::List,
        ClientRequest::Spawn { session, .. } => Operation::Spawn(session.id),
        ClientRequest::Attach { terminal_id, .. } => Operation::Attach(*terminal_id),
        ClientRequest::SetVisibility { terminal_id, .. } => Operation::Visibility(*terminal_id),
        ClientRequest::TerminalAcknowledge { terminal_id, .. } => {
            Operation::OutputAck(*terminal_id)
        }
        ClientRequest::Input { terminal_id, bytes } => {
            Operation::Input(*terminal_id, bytes.clone())
        }
        _ => Operation::Other,
    };
    let hooks = CONNECTION.with(|value| value.borrow().as_ref().unwrap().0.clone());
    if operation == Operation::Authenticate
        && hooks.lost_reply.load(Ordering::Acquire)
        && !hooks.paused_auth.swap(true, Ordering::AcqRel)
    {
        hooks.gated.send(client).unwrap();
        hooks
            .release
            .recv_timeout(Duration::from_secs(5))
            .expect("release real reconnect authentication");
    }
    // All admission, registry, original-source, PTY and delivery behavior is real.
    let result = process_request(host, client, request);
    hooks.observed.lock().unwrap().push(Observed {
        client,
        operation: operation.clone(),
        refused: result.as_ref().err().map(|error| error.code),
    });
    if matches!(&operation, Operation::Attach(_))
        && result.is_ok()
        && hooks.lost_reply.load(Ordering::Acquire)
        && !hooks.paused_source.swap(true, Ordering::AcqRel)
    {
        // Subscription/real source selection already happened, but the actual
        // Attached response has not been queued. No replacement reply is made.
        hooks.source_ready.send(client).unwrap();
        hooks
            .source_release
            .recv_timeout(Duration::from_secs(5))
            .expect("release exact source descriptor");
    }
    if matches!(&operation, Operation::Input(_, bytes) if bytes == FIRST)
        && result.is_ok()
        && hooks.lose_first_reply.swap(false, Ordering::AcqRel)
    {
        // The real PTY input owner accepted this request; the later file witness
        // proves its effect. Lose only the reply on this exact accepted stream,
        // before serve_with can queue it.
        hooks.lost_reply.store(true, Ordering::Release);
        CONNECTION.with(|value| {
            value
                .borrow()
                .as_ref()
                .unwrap()
                .1
                .shutdown(Shutdown::Both)
                .unwrap()
        });
    }
    result
}

struct PrivateServer {
    host: SharedHost,
    socket: PathBuf,
    proof: HostSessionProof,
    hooks: Arc<Hooks>,
    gated: crossbeam_channel::Receiver<ClientId>,
    release: crossbeam_channel::Sender<()>,
    source_ready: crossbeam_channel::Receiver<ClientId>,
    source_release: crossbeam_channel::Sender<()>,
    stop: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<UnixStream>>>,
    accept: Option<thread::JoinHandle<Vec<thread::JoinHandle<()>>>>,
    _lease: HostLease,
}

impl PrivateServer {
    fn new(root: &Path, lose_first_reply: bool) -> Self {
        let paths = RuntimePaths::discover().unwrap();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new(uuid::Uuid::new_v4().to_string()),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: HostIncarnation::new(uuid::Uuid::new_v4().to_string()),
        };
        let lease = HostLease::acquire(&paths, &proof).unwrap();
        let mut state = HostState::new(proof.clone());
        state.install_model(crate::host::model::Registry::at(&root.join("model.sqlite3")).unwrap());
        let host = Arc::new(HostLock::new(state));
        let listener = UnixListener::bind(&paths.socket).unwrap();
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (gate_tx, gated) = crossbeam_channel::bounded(1);
        let (release, release_rx) = crossbeam_channel::bounded(1);
        let (source_ready_tx, source_ready) = crossbeam_channel::bounded(1);
        let (source_release, source_release_rx) = crossbeam_channel::bounded(1);
        let hooks = Arc::new(Hooks {
            observed: Mutex::new(Vec::new()),
            lose_first_reply: AtomicBool::new(lose_first_reply),
            lost_reply: AtomicBool::new(false),
            paused_auth: AtomicBool::new(false),
            paused_source: AtomicBool::new(false),
            source_ready: source_ready_tx,
            source_release: source_release_rx,
            gated: gate_tx,
            release: release_rx,
        });
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let accept = {
            let (host, hooks, stop, connections) = (
                host.clone(),
                hooks.clone(),
                stop.clone(),
                connections.clone(),
            );
            thread::spawn(move || {
                let mut clients = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            assert!(clients.len() < 12, "unexpected reconnect loop");
                            let client = clients.len() as ClientId + 1;
                            connections
                                .lock()
                                .unwrap()
                                .push(stream.try_clone().unwrap());
                            let (host, hooks) = (host.clone(), hooks.clone());
                            clients.push(thread::spawn(move || {
                                CONNECTION.with(|value| {
                                    *value.borrow_mut() = Some((hooks, stream.try_clone().unwrap()))
                                });
                                serve_with(stream, client, host, observed_request);
                                CONNECTION.with(|value| value.borrow_mut().take());
                            }));
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1))
                        }
                        Err(error) => panic!("private accept failed: {error}"),
                    }
                }
                clients
            })
        };
        Self {
            host,
            socket: paths.socket,
            proof,
            hooks,
            gated,
            release,
            source_ready,
            source_release,
            stop,
            connections,
            accept: Some(accept),
            _lease: lease,
        }
    }

    fn fixture_user(&self) -> ControlClient {
        let mut client = ControlClient::connect(&self.socket).unwrap();
        // Predetermined synthetic fixture User bootstrap, never a fallback for
        // a refused capability or a substitute for the invoking session.
        client.authenticate(Authority::default()).unwrap();
        client
    }

    fn issued_authority(&self, source: TerminalId) -> Authority {
        // This capability was actually issued by the real framed Spawn. Keep
        // it opaque; no token/string/env extraction, serialization or printing.
        crate::host::agent_execution::registry(&self.host)
            .unwrap()
            .lock()
            .unwrap()
            .retained_agent_authority(source)
            .unwrap()
    }

    fn worker(&self, id: TerminalId) -> TerminalHandle {
        self.host.lock().unwrap().terminals[&id].handle.clone()
    }

    fn operations(&self) -> Vec<Observed> {
        self.hooks.observed.lock().unwrap().clone()
    }

    fn settle(&mut self) -> bool {
        let mut settled = true;
        self.stop.store(true, Ordering::Release);
        let _ = self.release.try_send(());
        let _ = self.source_release.try_send(());
        // First close admission and join the accept owner, so no last accepted
        // stream can appear after the exact connection cleanup inventory.
        let deadline = Instant::now() + Duration::from_secs(5);
        let clients = if let Some(accept) = self.accept.take() {
            if before(deadline, || accept.is_finished()) {
                match accept.join() {
                    Ok(clients) => clients,
                    Err(_) => {
                        settled = false;
                        Vec::new()
                    }
                }
            } else {
                settled = false;
                Vec::new()
            }
        } else {
            Vec::new()
        };
        for stream in self.connections.lock().unwrap().iter() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        for client in clients {
            if before(deadline, || client.is_finished()) {
                settled &= client.join().is_ok();
            } else {
                settled = false;
            }
        }
        let workers = self
            .host
            .lock()
            .unwrap()
            .terminals
            .values()
            .map(|record| record.handle.clone())
            .collect::<Vec<_>>();
        for worker in workers {
            settled &= worker.close_checked(true) == TerminalCloseResult::Closed;
            settled &= worker.is_completed();
        }
        settled
    }

    fn finish(&mut self) {
        assert!(
            self.settle(),
            "original private server/PTY owners did not settle"
        );
    }
}

impl Drop for PrivateServer {
    fn drop(&mut self) {
        // The same exact owned streams/workers are used even during unwinding.
        let _ = self.settle();
    }
}

fn before(deadline: Instant, condition: impl Fn() -> bool) -> bool {
    while !condition() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(1));
    }
    true
}

fn wait_until(description: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

fn terminal(root: &Path, name: &str) -> TerminalSession {
    let directory = root.join(name);
    fs::create_dir(&directory).unwrap();
    let recipe = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), "printf ready > ready; while IFS= read -r line; do printf '%s\\n' \"$line\" >> input; printf 'SEEN:%s\\n' \"$line\"; done".into()],
        working_directory: directory,
        environment: BTreeMap::new(),
        unset_environment: Default::default(),
    };
    TerminalSession {
        id: TerminalId::new(),
        launch: recipe.clone(),
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
    }
}

fn adapter(
    server: &PrivateServer,
    session: TerminalSession,
    authority: Authority,
) -> HostedTerminalHandle {
    HostedTerminalHandle::attach_or_spawn_at_with_authority(
        server.socket.clone(),
        session,
        TerminalSpawnOptions::default(),
        Some(server.proof.clone()),
        authority,
        |_| {},
    )
    .unwrap()
}

fn wait_error(handle: &HostedTerminalHandle) -> String {
    let events = handle.events();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("real refusal reaches adapter");
        if let TerminalEvent::Error { message, .. } = event {
            return message;
        }
        assert!(Instant::now() < deadline, "missing permanent refusal");
    }
}

#[test]
fn actual_server_reauthenticates_the_issued_caller_and_never_replays_accepted_pty_input() {
    let name = thread::current().name().unwrap().to_owned();
    let Some(root) = crate::host::exit_drain_tests::private_runtime_case(&name, None) else {
        return;
    };
    let mut server = PrivateServer::new(&root, true);
    let mut bootstrap = server.fixture_user();
    let caller = terminal(&root, "caller");
    bootstrap.spawn(caller.clone(), Default::default()).unwrap();
    let authority = server.issued_authority(caller.id);
    assert!(authority != Authority::default());
    let child = terminal(&root, "owned-child");
    let handle = adapter(&server, child.clone(), authority);
    handle.wait_until_attached(Duration::from_secs(5)).unwrap();
    wait_until("actual child shell", || {
        child.launch.working_directory.join("ready").exists()
    });
    let original = server.worker(child.id);
    handle.acknowledge_updates().unwrap();
    handle.input(FIRST).unwrap();
    let reconnect = server
        .gated
        .recv_timeout(Duration::from_secs(5))
        .expect("same adapter reconnects through real authentication");
    wait_until("first real PTY effect", || {
        fs::read(child.launch.working_directory.join("input"))
            .ok()
            .as_deref()
            == Some(FIRST)
    });
    handle.input(SECOND).unwrap();
    assert!(handle.wait_until_attached(Duration::ZERO).is_err());
    assert!(
        !server
            .host
            .lock()
            .unwrap()
            .agent_clients
            .contains_key(&reconnect)
    );
    assert!(
        server
            .operations()
            .iter()
            .all(|event| event.client != reconnect)
    );
    assert_eq!(
        fs::read(child.launch.working_directory.join("input")).unwrap(),
        FIRST
    );
    server.release.send(()).unwrap();
    assert_eq!(
        server
            .source_ready
            .recv_timeout(Duration::from_secs(5))
            .unwrap(),
        reconnect
    );
    assert!(handle.wait_until_attached(Duration::ZERO).is_err());
    let stream = server.connections.lock().unwrap()[reconnect as usize - 1]
        .try_clone()
        .unwrap();
    let mut poll = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // A finite negative observation of this exact connection: no visibility,
    // output ACK, snapshot or queued input may precede the held descriptor.
    // SAFETY: poll borrows the live owned stream and one initialized pollfd.
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 100) }, 0);
    assert_eq!(
        server
            .operations()
            .iter()
            .filter(|event| event.client == reconnect)
            .map(|event| event.operation.clone())
            .collect::<Vec<_>>(),
        vec![
            Operation::Authenticate,
            Operation::List,
            Operation::Attach(child.id)
        ]
    );
    server.source_release.send(()).unwrap();
    handle.wait_until_attached(Duration::from_secs(5)).unwrap();
    let expected = [FIRST, SECOND].concat();
    wait_until("queued input after real reauthentication", || {
        fs::read(child.launch.working_directory.join("input"))
            .ok()
            .as_deref()
            == Some(expected.as_slice())
    });
    assert!(server.worker(child.id).same_worker(&original));
    assert_eq!(
        server.host.lock().unwrap().agent_clients.get(&reconnect),
        Some(&Lineage(caller.id))
    );
    let operations = server.operations();
    let resumed = operations
        .iter()
        .filter(|event| event.client == reconnect)
        .map(|event| &event.operation)
        .collect::<Vec<_>>();
    assert_eq!(resumed[0], &Operation::Authenticate);
    assert_eq!(resumed[1], &Operation::List);
    assert_eq!(resumed[2], &Operation::Attach(child.id));
    assert!(
        !resumed
            .iter()
            .any(|operation| matches!(operation, Operation::Spawn(_)))
    );
    assert_eq!(
        operations
            .iter()
            .filter(|event| event.operation == Operation::Input(child.id, FIRST.to_vec()))
            .count(),
        1
    );
    assert_eq!(
        operations
            .iter()
            .filter(|event| event.operation == Operation::Input(child.id, SECOND.to_vec()))
            .count(),
        1
    );
    wait_until("real rendered PTY output", || {
        handle.acknowledge_redraw(handle.generation());
        handle
            .snapshot()
            .lines
            .iter()
            .flat_map(|row| row.cells.iter())
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .contains("SEEN:SECOND_AFTER_AUTH")
    });
    handle.shutdown().unwrap();
    wait_until("original child worker cleanup", || original.is_completed());
    // Shutdown deliberately retains this exact completed worker for history;
    // it is not a request to retire its descriptor. The ordinary daemon poll
    // does not remove these records either (close_terminal_attention contract).
    wait_until("actual completion delivery", || {
        server
            .host
            .lock()
            .unwrap()
            .replay_events(child.id, 0)
            .unwrap()
            .iter()
            .any(|(_, event)| matches!(event, WireTerminalEvent::Completed { .. }))
    });
    {
        let state = server.host.lock().unwrap();
        assert!(state.terminals[&child.id].handle.same_worker(&original));
        let ended = state.descriptor(reconnect, child.id).unwrap();
        assert_eq!(ended.session.id, child.id);
        assert_eq!(ended.session.launch, child.launch);
        assert!(matches!(ended.session.continuity,
            TerminalContinuity::RelaunchAvailable { ref recipe, .. } if recipe == &child.launch));
        assert!(
            state
                .history_handle(reconnect, child.id)
                .unwrap()
                .same_worker(&original)
        );
        assert!(matches!(
            state.surface_handle(reconnect, child.id),
            Err(ProtocolError {
                code: ProtocolErrorCode::TerminalUnavailable,
                retryable: false,
                ..
            })
        ));
    }
    // The public framed path must also refuse later input before any effect,
    // while leaving the ended original available for read-only history.
    handle
        .input(b"MUST_NOT_REACH_CLOSED_PTY\n".to_vec())
        .unwrap();
    let events = handle.events();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("closed PTY input refusal");
        if let TerminalEvent::InputRejected { operation, message } = event {
            assert_eq!(operation, "write terminal input");
            assert!(message.contains("this input was not sent"));
            break;
        }
        assert!(Instant::now() < deadline, "missing closed PTY refusal");
    }
    assert_eq!(
        fs::read(child.launch.working_directory.join("input")).unwrap(),
        expected
    );
    assert!(server.worker(child.id).same_worker(&original));
    handle.stop_for_test();
    drop(handle);
    drop(bootstrap);
    server.finish();
}

#[test]
fn actual_server_refuses_foreign_input_and_reused_or_changed_original_sources_before_effects() {
    let name = thread::current().name().unwrap().to_owned();
    let Some(root) = crate::host::exit_drain_tests::private_runtime_case(&name, None) else {
        return;
    };
    let mut server = PrivateServer::new(&root, false);
    let mut bootstrap = server.fixture_user();
    let caller = terminal(&root, "caller");
    let sibling = terminal(&root, "sibling");
    bootstrap.spawn(caller.clone(), Default::default()).unwrap();
    bootstrap
        .spawn(sibling.clone(), Default::default())
        .unwrap();
    let authority = server.issued_authority(caller.id);
    let original = server.worker(sibling.id);
    wait_until("unrelated real shell", || {
        sibling.launch.working_directory.join("ready").exists()
    });

    // Observation/attachment remains public, but genuine lineage admission
    // refuses the first attempted mutation of an unrelated actual PTY.
    let foreign = adapter(&server, sibling.clone(), authority.clone());
    foreign.wait_until_attached(Duration::from_secs(5)).unwrap();
    foreign.input(b"MUST_NOT_REACH_SIBLING\n".to_vec()).unwrap();
    assert!(wait_error(&foreign).contains("belongs to another session"));
    foreign.wait_until_attached(Duration::ZERO).unwrap();
    foreign.stop_for_test();
    let foreign_client = server
        .operations()
        .iter()
        .find(|event| {
            event.operation == Operation::Input(sibling.id, b"MUST_NOT_REACH_SIBLING\n".to_vec())
        })
        .unwrap()
        .client;
    wait_until("refused client's actual disconnect", || {
        !server
            .host
            .lock()
            .unwrap()
            .clients
            .contains_key(&foreign_client)
    });
    assert!(!sibling.launch.working_directory.join("input").exists());
    assert!(server.worker(sibling.id).same_worker(&original));
    assert!(server.operations().iter().any(|event| event.operation
        == Operation::Input(sibling.id, b"MUST_NOT_REACH_SIBLING\n".to_vec())
        && event.refused == Some(ProtocolErrorCode::PermissionDenied)));
    drop(foreign);

    // Exact original-recipe mismatch is detected from the real inventory.
    // No Attach, restore vote, output ACK or queued input reaches the server.
    let before = server.operations().len();
    let mut changed = sibling.clone();
    changed
        .launch
        .arguments
        .push("different-original-recipe".into());
    let mismatch = adapter(&server, changed, authority.clone());
    mismatch
        .input(b"MUST_NOT_REACH_CHANGED_SOURCE\n".to_vec())
        .unwrap();
    assert!(wait_error(&mismatch).contains("different original source"));
    assert!(mismatch.wait_until_attached(Duration::ZERO).is_err());
    mismatch.stop_for_test();
    let after = server.operations();
    let new = &after[before..];
    assert_eq!(
        new.iter().map(|event| &event.operation).collect::<Vec<_>>(),
        vec![&Operation::Authenticate, &Operation::List]
    );
    assert!(!sibling.launch.working_directory.join("input").exists());
    drop(mismatch);

    // Actual fresh-only admission rejects taking over a predecessor before
    // spawning, recording a lineage or executing the fixture program.
    let mut reused = terminal(&root, "forbidden-relaunch");
    reused.predecessor = Some(caller.id);
    let refused = adapter(&server, reused.clone(), authority);
    refused.input(b"MUST_NOT_START\n".to_vec()).unwrap();
    assert!(wait_error(&refused).contains("may start a new terminal but not relaunch one"));
    assert!(refused.wait_until_attached(Duration::ZERO).is_err());
    refused.stop_for_test();
    assert!(!reused.launch.working_directory.join("ready").exists());
    assert!(
        !server
            .host
            .lock()
            .unwrap()
            .terminals
            .contains_key(&reused.id)
    );
    assert!(
        !crate::host::agent_execution::registry(&server.host)
            .unwrap()
            .lock()
            .unwrap()
            .lineage_recorded(reused.id)
            .unwrap()
    );
    assert!(
        server
            .operations()
            .iter()
            .any(|event| event.operation == Operation::Spawn(reused.id)
                && event.refused == Some(ProtocolErrorCode::PermissionDenied))
    );
    assert_eq!(bootstrap.list().unwrap().len(), 2);
    assert!(!server.worker(caller.id).is_completed());
    assert!(!original.is_completed());
    drop(refused);
    drop(bootstrap);
    server.finish();
}
