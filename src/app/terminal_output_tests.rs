//! Real hosted-client delivery into Application::update; no daemon or PTY.
use super::worktree_ui::card_directory;
use super::*;
use crate::terminal::{
    RenderSnapshot,
    ipc::{
        ClientEnvelope, ClientRequest, FrameCodec, FrameError, MAX_FRAME_BYTES, PROTOCOL_VERSION,
        ProtocolError, ProtocolErrorCode, ServerEnvelope, ServerMessage, ServerResponse,
        TerminalDescriptor, TerminalSnapshot, WireTerminalEvent, write_frame,
    },
};
use std::{
    cell::Cell,
    os::unix::net::UnixListener,
    sync::atomic::{AtomicBool, Ordering},
    thread,
};

thread_local! {
    static PROJECTIONS: Cell<usize> = const { Cell::new(0) };
    static PROTECTIONS: Cell<usize> = const { Cell::new(0) };
    static OWNER_RESOLUTIONS: Cell<usize> = const { Cell::new(0) };
    static FOLDER_ROOT_REFRESHES: Cell<usize> = const { Cell::new(0) };
}
pub(super) fn record_projection() {
    PROJECTIONS.set(PROJECTIONS.get() + 1);
}
pub(super) fn record_protection() {
    PROTECTIONS.set(PROTECTIONS.get() + 1);
}
pub(super) fn record_owner_resolution() {
    OWNER_RESOLUTIONS.set(OWNER_RESOLUTIONS.get() + 1);
}
pub(super) fn record_folder_root_refresh() {
    FOLDER_ROOT_REFRESHES.set(FOLDER_ROOT_REFRESHES.get() + 1);
}
fn counts() -> (usize, usize) {
    (PROJECTIONS.get(), PROTECTIONS.get())
}
fn reset() {
    PROJECTIONS.set(0);
    PROTECTIONS.set(0);
    OWNER_RESOLUTIONS.set(0);
}

#[derive(Debug)]
enum ObservedInput {
    Connect,
    Paste(String),
    Input(Vec<u8>),
    Key(crate::terminal::KeyInput),
}

struct Peer {
    _directory: tempfile::TempDir,
    handle: HostedTerminalHandle,
    send: crossbeam_channel::Sender<ServerMessage>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    sequence: u64,
    reject_next_paste: Arc<AtomicBool>,
    reject_next_key: Arc<std::sync::Mutex<Option<ProtocolError>>>,
    observed: crossbeam_channel::Receiver<ObservedInput>,
}
impl Peer {
    fn new(session: TerminalSession, proof: HostSessionProof) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("output.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let id = session.id;
        let mut descriptor = TerminalDescriptor::relaunchable(id, session.launch.clone(), None);
        descriptor.session = session;
        descriptor.attached = true;
        let (send, messages) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let expected_proof = proof.clone();
        let reject_next_paste = Arc::new(AtomicBool::new(false));
        let reject_paste = Arc::clone(&reject_next_paste);
        let reject_next_key = Arc::new(std::sync::Mutex::new(None));
        let reject_key = Arc::clone(&reject_next_key);
        let (observe, observed) = crossbeam_channel::unbounded();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                if stopping.load(Ordering::Relaxed) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline);
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("private accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_millis(10)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
            let mut connected = false;
            while !stopping.load(Ordering::Relaxed) {
                for message in messages.try_iter() {
                    if write_frame(
                        &mut stream,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: None,
                            proof: proof.clone(),
                            message,
                        },
                    )
                    .is_err()
                    {
                        return;
                    }
                }
                let request: ClientEnvelope = match codec.read(&mut stream) {
                    Ok(Some(request)) => request,
                    Ok(None) => return,
                    Err(FrameError::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => panic!("private decode: {error}"),
                };
                // Observe only fixture input/connection kinds, never copy an
                // Authenticate request or its inherited opaque authority.
                let observed_request = match &request.request {
                    ClientRequest::Connect { .. } => Some(ObservedInput::Connect),
                    ClientRequest::Paste { text, .. } => Some(ObservedInput::Paste(text.clone())),
                    ClientRequest::Input { bytes, .. } => Some(ObservedInput::Input(bytes.clone())),
                    ClientRequest::Key { input, .. } => Some(ObservedInput::Key(input.clone())),
                    _ => None,
                };
                let message = if !connected {
                    assert!(matches!(request.request, ClientRequest::Connect { .. }));
                    connected = true;
                    ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: vec![descriptor.clone()],
                        },
                    }
                } else if matches!(request.request, ClientRequest::Paste { .. })
                    && reject_paste.swap(false, Ordering::Relaxed)
                {
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::QueueFull,
                        "the terminal command queue is full",
                        true,
                    ))
                } else if matches!(request.request, ClientRequest::Key { .. })
                    && let Some(error) = reject_key.lock().unwrap().take()
                {
                    ServerMessage::Error(error)
                } else if matches!(request.request, ClientRequest::CopyTerminalSelection { .. }) {
                    // Control errors carry the request ID. The hosted client
                    // restores the exact pending operation before UI delivery.
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::InvalidRequest,
                        "fixture copy refused",
                        false,
                    ))
                } else {
                    ServerMessage::Response {
                        response: ServerResponse::Accepted,
                    }
                };
                if write_frame(
                    &mut stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request.request_id),
                        proof: proof.clone(),
                        message,
                    },
                )
                .is_err()
                {
                    return;
                }
                if let Some(request) = observed_request {
                    observe.send(request).unwrap();
                }
            }
        });
        let handle =
            HostedTerminalHandle::connect_at(socket, id, Some(expected_proof), |_| {}).unwrap();
        let peer = Self {
            _directory: directory,
            handle,
            send,
            stop,
            worker: Some(worker),
            sequence: 0,
            reject_next_paste,
            reject_next_key,
            observed,
        };
        peer.handle
            .wait_until_attached(Duration::from_secs(3))
            .unwrap();
        peer
    }
    fn publish(&mut self, event: Option<WireTerminalEvent>) -> u64 {
        self.sequence += 1;
        let id = self.handle.terminal_id();
        if let Some(event) = event {
            self.send
                .send(ServerMessage::Event {
                    terminal_id: id,
                    sequence: self.sequence,
                    event,
                })
                .unwrap();
        }
        let mut snapshot = RenderSnapshot::empty();
        snapshot.generation = self.sequence;
        snapshot.activity = Some(crate::model::TerminalOutputActivity {
            sequence: self.sequence,
            last_output_at_unix_ms: 0,
        });
        self.send
            .send(ServerMessage::Snapshot {
                terminal_id: id,
                snapshot: TerminalSnapshot::full(&snapshot),
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.handle.generation() < self.sequence {
            assert!(Instant::now() < deadline, "actual hosted delivery");
            thread::sleep(Duration::from_millis(1));
        }
        self.sequence
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn fixture() -> (Cagents, CardId, Peer) {
    let mut app = super::tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let CardKind::Terminal(mut session) = app.card_state(card).unwrap().kind.clone() else {
        panic!()
    };
    let proof = HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("output-test"),
        boot_id: crate::model::BootId::new("output-test"),
        host_incarnation: crate::model::HostIncarnation::new("output-test"),
    };
    session.continuity = TerminalContinuity::Live {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    app.card_state_mut(card).unwrap().kind = CardKind::Terminal(session.clone());
    let peer = Peer::new(session, proof);
    app.cards
        .insert(card, CardRuntime::Terminal(peer.handle.clone().into()));
    drop(app.update(Message::TerminalWake {
        card,
        generation: 0,
    }));
    reset();
    (app, card, peer)
}
fn apply(app: &mut Cagents, card: CardId, peer: &mut Peer, event: Option<WireTerminalEvent>) {
    let generation = peer.publish(event);
    drop(app.update(Message::TerminalWake { card, generation }));
}

#[test]
fn grid_output_keeps_workspace_and_protection_preparation_idle() {
    let (mut app, card, mut peer) = fixture();
    let template = app.card_state(card).unwrap().clone();
    for index in 0..200 {
        let mut cold = template.clone();
        cold.id = CardId::new();
        cold.exposure = Exposure::Offscreen;
        cold.residency = Residency::Cold;
        Arc::make_mut(&mut app.durable).cards.push(cold);
        app.known_worktree_roots
            .insert(PathBuf::from(format!("/output-fixture/root-{index}")));
    }
    let durable = Arc::clone(&app.durable);
    app.status = Some(TransientNotice::warning("output test notice"));
    for _ in 0..32 {
        apply(&mut app, card, &mut peer, None);
    }
    assert_eq!(counts(), (0, 0));
    assert!(Arc::ptr_eq(&durable, &app.durable));
    assert_eq!(peer.handle.snapshot().generation, 32);
    assert!(
        app.notices
            .entries
            .iter()
            .any(|notice| notice.notice.message == "output test notice")
    );
}

#[test]
fn semantic_terminal_changes_invalidate_but_identical_reports_do_not() {
    let (mut app, card, mut peer) = fixture();
    let events = [
        WireTerminalEvent::TitleChanged {
            title: "output agent".into(),
        },
        WireTerminalEvent::WorkingDirectoryChanged {
            path: "/output-fixture/new".into(),
        },
        WireTerminalEvent::ResumeHintChanged {
            hint: Some("resume-output".into()),
        },
        WireTerminalEvent::AgentActivityChanged {
            activity: Some(crate::agent::Activity {
                provider: crate::agent::Provider::Claude,
                state: crate::agent::ActivityState::Working,
            }),
        },
    ];
    for event in events {
        reset();
        apply(&mut app, card, &mut peer, Some(event.clone()));
        assert_eq!(counts(), (1, 1), "{event:?}");
        reset();
        let durable = Arc::clone(&app.durable);
        apply(&mut app, card, &mut peer, Some(event));
        assert_eq!(counts(), (0, 0));
        assert!(Arc::ptr_eq(&durable, &app.durable));
    }
    reset();
    apply(&mut app, card, &mut peer, Some(WireTerminalEvent::Bell));
    assert_eq!(counts(), (1, 1));
    reset();
    apply(
        &mut app,
        card,
        &mut peer,
        Some(WireTerminalEvent::Completed {
            exit_code: 0,
            signal: None,
            intentional: false,
        }),
    );
    assert_eq!(counts(), (1, 1));
    assert!(ended_terminal(app.card_state(card)).is_some());
}

#[test]
fn clipboard_reply_is_delivered_even_when_no_metadata_changed() {
    use cosmic::iced::futures::StreamExt;
    let (mut app, card, mut peer) = fixture();
    let request = uuid::Uuid::new_v4();
    app.terminal_find.entry(card).or_default().pending_copy = Some(request);
    let generation = peer.publish(Some(WireTerminalEvent::ClipboardText {
        request,
        text: "copied output".into(),
    }));
    let task = app.update(Message::TerminalWake { card, generation });
    assert_eq!(counts(), (0, 0));
    assert_eq!(app.terminal_find[&card].pending_copy, None);
    let mut stream = iced::runtime::task::into_stream(task).expect("copy task retained");
    let action = cosmic::iced::futures::executor::block_on(stream.next()).unwrap();
    assert!(matches!(action, iced::runtime::Action::Clipboard(
        iced::runtime::clipboard::Action::Write { contents, .. }
    ) if contents == "copied output"));
}

#[tokio::test]
async fn grid_output_still_schedules_live_find_refresh() {
    use cosmic::iced::futures::StreamExt;
    let (mut app, card, mut peer) = fixture();
    app.terminal_find.entry(card).or_default().open = true;
    drop(app.handle_terminal_find(terminal_ui::Action::Query(card, "needle".into())));
    reset();
    let generation = peer.publish(None);
    let task = app.update(Message::TerminalWake { card, generation });
    assert_eq!(counts(), (0, 0));
    let mut stream = iced::runtime::task::into_stream(task).expect("Find timer retained");
    let refreshed = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(action) = stream.next().await {
            if matches!(action, iced::runtime::Action::Output(cosmic::Action::App(
                Message::TerminalFind(terminal_ui::Action::Refresh(target, _))
            )) if target == card)
            {
                return true;
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(refreshed);
}

#[test]
fn root_exit_keeps_the_live_descendant_surface_and_controls() {
    let (mut app, card, mut peer) = fixture();
    apply(
        &mut app,
        card,
        &mut peer,
        Some(WireTerminalEvent::Exited {
            exit_code: 0,
            signal: None,
            intentional: false,
        }),
    );
    assert_eq!(counts(), (0, 0));
    assert!(ended_terminal(app.card_state(card)).is_none());
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Terminal(_))
    ));
    apply(
        &mut app,
        card,
        &mut peer,
        Some(WireTerminalEvent::TitleChanged {
            title: "descendant still alive".into(),
        }),
    );
    assert_eq!(counts(), (1, 1));
}

#[test]
fn terminal_control_failure_reports_a_notice_and_clears_pending_copy() {
    let (mut app, card, peer) = fixture();
    let request = uuid::Uuid::new_v4();
    app.terminal_find.entry(card).or_default().pending_copy = Some(request);
    peer.handle
        .copy_selection(
            request,
            crate::terminal::SearchMatch {
                start_row: 0,
                start_column: 0,
                end_row: 0,
                end_column: 1,
            },
        )
        .unwrap();
    let events = peer.handle.events();
    let deadline = Instant::now() + Duration::from_secs(3);
    while events.is_empty() {
        assert!(
            Instant::now() < deadline,
            "actual failed Copy request reply"
        );
        thread::sleep(Duration::from_millis(1));
    }
    drop(app.update(Message::TerminalWake {
        card,
        generation: peer.handle.generation(),
    }));
    assert_eq!(counts(), (1, 1));
    assert_eq!(app.terminal_find[&card].pending_copy, None);
    assert!(
        app.notices
            .entries
            .iter()
            .any(|notice| notice.notice.message == "fixture copy refused")
    );
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Terminal(_))
    ));
}

#[test]
fn confirmed_host_proof_refreshes_uncertain_terminal_metadata() {
    let (mut app, card, mut peer) = fixture();
    app.uncertain_terminal_proofs
        .insert(card, peer.handle.proof().unwrap());
    apply(&mut app, card, &mut peer, None);
    assert_eq!(counts(), (1, 1));
    assert!(!app.uncertain_terminal_proofs.contains_key(&card));
    reset();
    apply(&mut app, card, &mut peer, None);
    assert_eq!(counts(), (0, 0));
}

#[test]
fn indexed_owners_preserve_duplicate_rows_folder_precedence_and_explicit_filing() {
    use crate::{
        agent::{Activity, ActivityState, Provider},
        model::{ProjectKind, WorktreeId},
        repository::RepositoryId,
        sidebar::{SidebarProject, SidebarWorktree},
    };
    let mut app = super::tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let root = card_directory(app.card_state(card).unwrap())
        .unwrap()
        .to_path_buf();
    let nested = root.join("nested-owner");
    let first = ProjectId::new();
    let second = ProjectId::new();
    let first_row = WorktreeId::new();
    let second_row = WorktreeId::new();
    let nested_row = WorktreeId::new();
    fn row(id: WorktreeId, path: PathBuf) -> SidebarWorktree {
        SidebarWorktree::new(id, path, None, false, false, None, None, None)
    }
    app.sidebar_model.projects = vec![
        SidebarProject::new(
            first,
            RepositoryId::new(),
            "Folder".into(),
            root.clone(),
            vec![row(first_row, root.clone())],
            vec![],
        )
        .with_kind(ProjectKind::Folder),
        SidebarProject::new(
            second,
            RepositoryId::new(),
            "Repository".into(),
            root.clone(),
            vec![
                row(second_row, root.clone()),
                row(nested_row, nested.clone()),
            ],
            vec![],
        ),
    ];
    let folder = app.sidebar_model.projects[0].folder_key().unwrap();
    let working = Activity {
        provider: Provider::Claude,
        state: ActivityState::Working,
    };
    app.sidebar_state.set_card_activity(card, working);
    app.sidebar_state.observe_card_update(card);
    drop(app.update(Message::WindowResized(1200.0, 800.0)));
    assert_eq!(
        app.sidebar_state.agent_activity(first, folder.worktree_id),
        Some(ActivityState::Working)
    );
    assert_eq!(app.sidebar_state.agent_activity(first, first_row), None);
    assert_eq!(app.sidebar_state.agent_activity(second, second_row), None);
    assert!(app.sidebar_state.worktree_has_panes(first, first_row));
    assert!(app.sidebar_state.worktree_has_panes(second, second_row));
    app.worktree_owners.insert(card, nested.clone());
    Arc::make_mut(&mut app.durable)
        .card_scopes
        .insert(card, root.clone());
    drop(app.update(Message::WindowResized(1200.0, 800.0)));
    assert_eq!(
        app.sidebar_state.agent_activity(first, folder.worktree_id),
        None
    );
    assert_eq!(
        app.sidebar_state.agent_activity(second, nested_row),
        None,
        "ambiguous durable filing retains the current conversation projection policy"
    );
    assert!(!app.sidebar_state.worktree_has_panes(first, first_row));
    assert!(app.sidebar_state.worktree_has_panes(second, nested_row));
    Arc::make_mut(&mut app.durable).card_scopes.remove(&card);
    drop(app.update(Message::WindowResized(1200.0, 800.0)));
    assert_eq!(
        app.sidebar_state.agent_activity(second, nested_row),
        Some(ActivityState::Working)
    );
    assert!(app.sidebar_state.has_agent_update(second, nested_row));
    assert!(!app.sidebar_state.worktree_has_panes(first, first_row));
    assert!(app.sidebar_state.worktree_has_panes(second, nested_row));
    Arc::make_mut(&mut app.durable)
        .channels
        .push(crate::model::ProjectChannel {
            id: crate::model::ChannelId::new(),
            project_id: second,
            title: "Channeled".into(),
            title_source: Default::default(),
            ordinal: 0,
            shared: false,
            cards: vec![card],
            focused: Some(card),
        });
    drop(app.update(Message::WindowResized(1200.0, 800.0)));
    assert!(!app.sidebar_state.worktree_has_panes(second, nested_row));
    assert_eq!(app.sidebar_state.agent_activity(second, nested_row), None);
    assert!(app.sidebar_state.has_card_update(card));
    Arc::make_mut(&mut app.durable).channels.clear();
    app.worktree_owners.remove(&card);
    Arc::make_mut(&mut app.durable).card_scopes.remove(&card);
    app.known_worktree_roots.clear();
    for project in &mut app.sidebar_model.projects {
        project.worktrees.clear();
    }
    if let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind {
        session.launch.working_directory = root.join("unregistered-child");
    }
    drop(app.update(Message::WindowResized(1200.0, 800.0)));
    assert_eq!(
        app.sidebar_state.agent_activity(first, folder.worktree_id),
        None,
        "an exact folder key is not a newly discovered ancestor root"
    );
    assert!(app.sidebar_state.has_card_update(card));
    assert_eq!(app.sidebar_state.card_activity(card), Some(working));
}

#[test]
fn equal_full_reads_and_view_acknowledgements_reconcile_but_stale_reads_do_not() {
    use crate::workspace_model::{WorkspaceModel, views::Scope};
    let mut app = super::tests::context_inspector_test_app(false);
    let model = WorkspaceModel {
        identity: uuid::Uuid::new_v4(),
        sequence: 3,
        ..WorkspaceModel::default()
    };
    drop(app.update(Message::WorkspaceModelObserved(
        workspace_model_sync::ModelUpdate::Connected(model.clone()),
    )));
    reset();
    let durable = Arc::clone(&app.durable);
    drop(app.update(Message::WorkspaceModelObserved(
        workspace_model_sync::ModelUpdate::Read(model.clone()),
    )));
    assert_eq!(counts(), (1, 1), "equal models can repair local projection");
    assert_eq!(durable.channels, app.durable.channels);
    reset();
    let mut stale = model.clone();
    stale.sequence -= 1;
    drop(app.update(Message::WorkspaceModelObserved(
        workspace_model_sync::ModelUpdate::Read(stale),
    )));
    assert_eq!(counts(), (0, 0));
    drop(app.update(Message::WorkspaceModelObserved(
        workspace_model_sync::ModelUpdate::ViewApplied {
            scope: Scope::Channel(crate::model::ChannelId::new()),
            operation: Some(uuid::Uuid::new_v4()),
            model,
        },
    )));
    assert_eq!(
        counts(),
        (1, 1),
        "equal contents cannot suppress the acknowledgement handler"
    );
}

#[test]
fn a_folded_agents_edge_does_not_hide_the_same_sequence_full_inventory() {
    use crate::{
        agent::Provider,
        agent_execution::{Agent, Origin, Spec, Surface, Target},
        workspace_model::{ModelEdit, WorkspaceModel, views::Scope},
    };
    let mut app = super::tests::context_inspector_test_app(false);
    let model = WorkspaceModel {
        identity: uuid::Uuid::new_v4(),
        sequence: 3,
        ..WorkspaceModel::default()
    };
    drop(app.update(Message::WorkspaceModelObserved(
        workspace_model_sync::ModelUpdate::Connected(model),
    )));
    drop(app.update(Message::WorkspaceModelObserved(
        workspace_model_sync::ModelUpdate::Edited {
            sequence: 4,
            edit: ModelEdit::AgentsChanged,
        },
    )));
    assert_eq!(app.workspace_model.sequence, 4);
    assert!(app.workspace_model.agents.is_empty());
    let mut full = app.workspace_model.clone();
    let path = PathBuf::from("/inventory-fixture");
    full.agents.push(Agent {
        id: crate::model::AgentId::new(),
        name: None,
        origin: Origin::User,
        spec: Spec {
            provider: Provider::Claude,
            account: None,
            target: Target {
                repository: path.clone(),
                directory: path.clone(),
                placement: None,
            },
            surface: Surface::Chat,
            model: None,
            permission_mode: None,
            reasoning_effort: None,
        },
        placement: Scope::Worktree {
            repository: path.clone(),
            path,
        },
        session: None,
        console: None,
        created_ms: 0,
        closed: true,
    });
    reset();
    drop(app.update(Message::WorkspaceModelObserved(
        workspace_model_sync::ModelUpdate::Read(full.clone()),
    )));
    assert_eq!(app.workspace_model, full);
    assert_eq!(
        counts(),
        (1, 1),
        "folding the edge did not provide its inventory"
    );
    reset();
    drop(app.update(Message::WorkspaceModelObserved(
        workspace_model_sync::ModelUpdate::Read(full),
    )));
    assert_eq!(
        counts(),
        (1, 1),
        "equal inventory still reconciles local state"
    );
}

#[test]
fn older_complete_inventory_after_newer_metadata_edge_still_projects() {
    use crate::workspace_model::{
        ModelEdit, WorkspaceModel,
        views::{Scope, View},
    };
    let mut app = super::tests::context_inspector_test_app(false);
    let initial = WorkspaceModel {
        identity: uuid::Uuid::new_v4(),
        sequence: 3,
        ..WorkspaceModel::default()
    };
    drop(app.update(Message::WorkspaceModelObserved(
        workspace_model_sync::ModelUpdate::Connected(initial.clone()),
    )));
    drop(app.update(Message::WorkspaceModelObserved(
        workspace_model_sync::ModelUpdate::Edited {
            sequence: 5,
            edit: ModelEdit::AgentsChanged,
        },
    )));
    let mut inventory = initial;
    inventory.sequence = 4;
    inventory
        .views
        .push(View::empty(Scope::Channel(crate::model::ChannelId::new())));
    reset();
    drop(app.update(Message::WorkspaceModelObserved(
        workspace_model_sync::ModelUpdate::Read(inventory.clone()),
    )));
    assert_eq!(app.workspace_model.sequence, 5);
    assert_eq!(app.workspace_model.views, inventory.views);
    assert_eq!(app.shared_views.snapshot_sequence, 4);
    assert_eq!(
        counts(),
        (1, 1),
        "new collection evidence must reach projection"
    );
    reset();
    inventory.sequence = 3;
    inventory.views.clear();
    drop(app.update(Message::WorkspaceModelObserved(
        workspace_model_sync::ModelUpdate::Read(inventory),
    )));
    assert_eq!(app.workspace_model.views.len(), 1);
    assert_eq!(
        counts(),
        (0, 0),
        "inventory older than the complete snapshot is stale"
    );
}

#[test]
fn refused_optimistic_channel_title_is_restored_by_equal_authoritative_read() {
    use crate::{
        model::{ChannelId, ChannelTitleSource, ProjectChannel},
        repository::RepositoryId,
        sidebar::SidebarProject,
        workspace_model::{
            ModelChannel, WorkspaceModel,
            views::{Pane, Scope, View},
        },
    };

    // Also cover a source-only upgrade: equal captions do not make an invalid
    // Provider attribution authoritative after a refused suggestion.
    for suggested in ["Refused provider title", "Original prompt title"] {
        let mut app = super::tests::context_inspector_test_app(true);
        let project = ProjectId::new();
        let channel = ChannelId::new();
        let repository = PathBuf::from("/title-refusal-fixture/.git");
        let mut chat = app.durable.cards[0].clone();
        chat.id = CardId::new();
        chat.exposure = Exposure::Offscreen;
        chat.residency = Residency::Cold;
        chat.kind = CardKind::Chat(crate::chat::Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: crate::model::TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Omp,
                recipe: crate::model::LaunchRecipe {
                    unset_environment: Default::default(),
                    program: "/bin/sh".into(),
                    arguments: Vec::new(),
                    working_directory: "/title-refusal-fixture".into(),
                    environment: Default::default(),
                },
                session_id: None,
            },
            proof: None,
            terminal: None,
        });
        let pane = Pane::from_card(&chat, Some(repository.clone())).unwrap();
        let card = chat.id;
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards.push(chat);
        durable.channels.push(ProjectChannel {
            id: channel,
            project_id: project,
            title: "Original prompt title".into(),
            title_source: ChannelTitleSource::Prompt,
            ordinal: 1,
            shared: true,
            cards: vec![card],
            focused: Some(card),
        });
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "Title refusal fixture".into(),
            repository.clone(),
            Vec::new(),
            Vec::new(),
        ));
        let authoritative = WorkspaceModel {
            identity: uuid::Uuid::new_v4(),
            sequence: 3,
            channels: vec![ModelChannel {
                id: channel,
                repository,
                title: "Original prompt title".into(),
                title_source: ChannelTitleSource::Prompt,
                ordinal: 1,
            }],
            views: vec![View {
                scope: Scope::Channel(channel),
                revision: 1,
                panes: vec![pane],
                retired: Vec::new(),
            }],
            ..WorkspaceModel::default()
        };
        app.workspace_model = authoritative.clone();
        app.shared_views.snapshot_sequence = authoritative.sequence;
        app.refresh_sidebar_channels();

        // Use the actual optimistic production path, including its persist and
        // row refresh, then deliver the publisher's refusal Read via App update.
        assert!(app.suggest_channel_title(card, suggested.into(), ChannelTitleSource::Provider));
        assert_eq!(app.workspace_model, authoritative);
        assert_eq!(app.durable.channels[0].title, suggested);
        assert_eq!(
            app.durable.channels[0].title_source,
            ChannelTitleSource::Provider
        );
        assert_eq!(app.sidebar_model.projects[0].channels[0].title, suggested);
        let optimistic = app.persistent_workspace();
        assert_eq!(optimistic.channels[0].title, suggested);
        assert_eq!(
            optimistic.channels[0].title_source,
            ChannelTitleSource::Provider
        );

        reset();
        drop(app.update(Message::WorkspaceModelObserved(
            workspace_model_sync::ModelUpdate::Read(authoritative.clone()),
        )));
        assert_eq!(counts(), (1, 1), "equal Read must repair local optimism");
        assert_eq!(app.workspace_model, authoritative);
        assert_eq!(app.durable.channels[0].title, "Original prompt title");
        assert_eq!(
            app.durable.channels[0].title_source,
            ChannelTitleSource::Prompt
        );
        assert_eq!(
            app.sidebar_model.projects[0].channels[0].title,
            "Original prompt title"
        );
        let restored = app.persistent_workspace();
        assert_eq!(restored.channels[0].title, "Original prompt title");
        assert_eq!(
            restored.channels[0].title_source,
            ChannelTitleSource::Prompt
        );
        assert!(!Arc::ptr_eq(&optimistic, &restored));
        assert_eq!(
            optimistic.channels[0].title_source,
            ChannelTitleSource::Provider
        );
    }
}

#[test]
fn unchanged_agent_output_reuses_ownership_and_topology_rebinds_attention() {
    use crate::{
        agent::{Activity, ActivityState, Provider},
        model::WorktreeId,
        repository::RepositoryId,
        sidebar::{SidebarProject, SidebarWorktree},
    };
    let (mut app, card, mut peer) = fixture();
    let root = card_directory(app.card_state(card).unwrap())
        .unwrap()
        .to_path_buf();
    let first = ProjectId::new();
    let first_row = WorktreeId::new();
    let second = ProjectId::new();
    let second_row = WorktreeId::new();
    let project = |id, row| {
        SidebarProject::new(
            id,
            RepositoryId::new(),
            "Agent output ownership".into(),
            root.clone(),
            vec![SidebarWorktree::new(
                row,
                root.clone(),
                None,
                false,
                false,
                None,
                None,
                None,
            )],
            vec![],
        )
    };
    app.sidebar_model.projects = vec![project(first, first_row)];
    app.worktree_owners.remove(&card);
    Arc::make_mut(&mut app.durable).card_scopes.remove(&card);
    app.known_worktree_roots.clear();
    for index in 0..200 {
        app.known_worktree_roots
            .insert(PathBuf::from(format!("/agent-output-fixture/root-{index}")));
    }
    // Output from a background window remains unread; no focus or navigation
    // action is needed to exercise its real sidebar ownership.
    app.refresh_ui.focused = false;
    let working = Activity {
        provider: Provider::Claude,
        state: ActivityState::Working,
    };
    apply(
        &mut app,
        card,
        &mut peer,
        Some(WireTerminalEvent::AgentActivityChanged {
            activity: Some(working),
        }),
    );
    assert_eq!(app.sidebar_state.card_activity(card), Some(working));
    assert_eq!(
        app.sidebar_state.agent_activity(first, first_row),
        Some(ActivityState::Working)
    );
    apply(&mut app, card, &mut peer, Some(WireTerminalEvent::Bell));
    assert!(app.sidebar_state.has_agent_update(first, first_row));
    let proof = peer.handle.proof();
    let durable = Arc::clone(&app.durable);
    reset();
    for _ in 0..32 {
        apply(&mut app, card, &mut peer, None);
    }
    apply(
        &mut app,
        card,
        &mut peer,
        Some(WireTerminalEvent::AgentActivityChanged {
            activity: Some(working),
        }),
    );
    assert_eq!(OWNER_RESOLUTIONS.get(), 0);
    assert_eq!(counts(), (0, 0));
    assert!(Arc::ptr_eq(&durable, &app.durable));
    assert_eq!(peer.handle.proof(), proof);
    assert_eq!(app.sidebar_state.card_activity(card), Some(working));
    assert!(app.sidebar_state.has_agent_update(first, first_row));

    // OSC cwd is live metadata, not a new launch/filing authority. It still
    // runs the structural tail without moving this unchanged activity.
    apply(
        &mut app,
        card,
        &mut peer,
        Some(WireTerminalEvent::WorkingDirectoryChanged {
            path: root.join("shell-only-directory"),
        }),
    );
    assert_eq!(counts(), (1, 1));
    assert_eq!(OWNER_RESOLUTIONS.get(), 0);
    assert_eq!(
        app.sidebar_state.agent_activity(first, first_row),
        Some(ActivityState::Working)
    );
    assert!(app.sidebar_state.has_agent_update(first, first_row));

    // A real structural update rebinds both cached activity and unread
    // attention even though no new activity event arrives from the terminal.
    app.sidebar_model.projects = vec![project(second, second_row)];
    drop(app.update(Message::WindowResized(1200.0, 800.0)));
    assert_eq!(app.sidebar_state.agent_activity(first, first_row), None);
    assert!(!app.sidebar_state.has_agent_update(first, first_row));
    assert_eq!(
        app.sidebar_state.agent_activity(second, second_row),
        Some(ActivityState::Working)
    );
    assert!(app.sidebar_state.has_agent_update(second, second_row));
    reset();
    apply(&mut app, card, &mut peer, None);
    assert_eq!(OWNER_RESOLUTIONS.get(), 0);
    assert_eq!(counts(), (0, 0));
    assert!(app.sidebar_state.has_agent_update(second, second_row));

    // Actual agent transitions must still use the current owner and publish
    // their existing activity/unread semantics after the unchanged fast path.
    for state in [
        ActivityState::Waiting,
        ActivityState::Working,
        ActivityState::Idle,
    ] {
        reset();
        apply(
            &mut app,
            card,
            &mut peer,
            Some(WireTerminalEvent::AgentActivityChanged {
                activity: Some(Activity { state, ..working }),
            }),
        );
        assert!(OWNER_RESOLUTIONS.get() > 0, "{state:?}");
        assert_eq!(counts(), (1, 1), "{state:?}");
        assert_eq!(
            app.sidebar_state.card_activity(card),
            Some(Activity { state, ..working }),
            "exact per-card activity survives every transition, including Idle"
        );
        // Worktree attention aggregates Working/Waiting, not idle agents.
        // Idle remains an exact per-card observation and does not read news.
        assert_eq!(
            app.sidebar_state.agent_activity(second, second_row),
            matches!(state, ActivityState::Working | ActivityState::Waiting).then_some(state)
        );
        assert_eq!(app.sidebar_state.agent_activity(first, first_row), None);
        assert!(!app.sidebar_state.has_agent_update(first, first_row));
        assert!(app.sidebar_state.has_agent_update(second, second_row));
        assert_eq!(peer.handle.proof(), proof);
    }
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Terminal(_))
    ));
}

#[test]
fn rejected_paste_keeps_the_actual_app_surface_proof_output_and_next_control() {
    let (mut app, card, mut peer) = fixture();
    let proof = peer.handle.proof().unwrap();
    let continuity = terminal_continuity(app.card_state(card)).unwrap().clone();
    let recovery_attempts = app.terminal_recovery_attempts.clone();
    let uncertain = app.uncertain_terminal_proofs.clone();
    let mut requests = peer.observed.try_iter().collect::<Vec<_>>();
    peer.reject_next_paste.store(true, Ordering::Relaxed);
    app.settings.confirm_multiline_paste = false;
    drop(app.update(Message::TerminalFind(terminal_ui::Action::Paste(
        card,
        "refused original input".into(),
    ))));
    // The normal user Paste update performs its existing projection. Measure
    // only the following typed rejection wake, not that setup work.
    let durable = Arc::clone(&app.durable);
    reset();
    let events = peer.handle.events();
    let deadline = Instant::now() + Duration::from_secs(3);
    while events.is_empty() {
        assert!(Instant::now() < deadline, "actual correlated paste refusal");
        thread::sleep(Duration::from_millis(1));
    }
    drop(app.update(Message::TerminalWake {
        card,
        generation: peer.handle.generation(),
    }));
    assert_eq!(counts(), (0, 0), "rejection changes only a notice");
    assert!(Arc::ptr_eq(&durable, &app.durable));
    assert_eq!(terminal_continuity(app.card_state(card)), Some(&continuity));
    assert_eq!(app.terminal_recovery_attempts, recovery_attempts);
    assert_eq!(app.uncertain_terminal_proofs, uncertain);
    assert!(
        matches!(app.cards.get(&card), Some(CardRuntime::Terminal(surface))
        if surface.terminal_id() == peer.handle.terminal_id() && surface.host_proof() == Some(proof.clone()))
    );
    assert!(ended_terminal(app.card_state(card)).is_none());
    assert!(app.notices.entries.iter().any(|entry| {
        entry.notice.severity == NoticeSeverity::Warning
            && entry.notice.message.contains("input queue is full")
            && entry.notice.message.contains("try again")
    }));
    // New output is delivered by this same open socket after the refused
    // command, and a distinct user command still reaches that exact peer.
    let generation = peer.publish(None);
    drop(app.update(Message::TerminalWake { card, generation }));
    assert_eq!(peer.handle.snapshot().generation, generation);
    peer.handle.input(b"after refusal".to_vec()).unwrap();
    loop {
        let request = peer.observed.recv_timeout(Duration::from_secs(3)).unwrap();
        let done = matches!(&request, ObservedInput::Input(bytes) if bytes == b"after refusal");
        requests.push(request);
        if done {
            break;
        }
    }
    requests.extend(peer.observed.try_iter());
    assert_eq!(
        requests
            .iter()
            .filter(|r| matches!(r, ObservedInput::Connect))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| matches!(r, ObservedInput::Paste(text)
        if text == "refused original input"))
            .count(),
        1,
        "never automatically replay refused input"
    );
    assert!(
        !peer.worker.as_ref().unwrap().is_finished(),
        "the same connection survives"
    );
    assert_eq!(peer.handle.proof(), Some(proof));
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Terminal(_))
    ));
}

#[test]
fn delayed_input_rejection_cannot_replace_a_newer_fatal_or_completed_terminal_outcome() {
    for completed in [false, true] {
        let (mut app, card, mut peer) = fixture();
        peer.reject_next_paste.store(true, Ordering::Relaxed);
        // A real server event precedes the correlated rejection on this same
        // framed connection. This also models a coalesced rejection released
        // after an already queued, newer terminal outcome.
        let outcome = if completed {
            WireTerminalEvent::Completed {
                exit_code: 7,
                signal: None,
                intentional: false,
            }
        } else {
            WireTerminalEvent::Error {
                operation: "read PTY".into(),
                message: "fixture fatal outcome".into(),
            }
        };
        let generation = peer.publish(Some(outcome));
        peer.handle
            .paste("stale input", crate::terminal::PasteOrigin::Clipboard)
            .unwrap();
        loop {
            let request = peer.observed.recv_timeout(Duration::from_secs(3)).unwrap();
            if matches!(request, ObservedInput::Paste(ref text) if text == "stale input") {
                break;
            }
        }
        let events = peer.handle.events();
        let deadline = Instant::now() + Duration::from_secs(3);
        while events.len() < 2 {
            assert!(
                Instant::now() < deadline,
                "both actual terminal events must arrive"
            );
            thread::yield_now();
        }
        app.status = Some(TransientNotice::warning("preserve existing notice"));
        drop(app.update(Message::TerminalWake { card, generation }));
        if completed {
            assert!(ended_terminal(app.card_state(card)).is_some());
            assert!(
                app.notices
                    .entries
                    .iter()
                    .any(|entry| entry.notice.severity == NoticeSeverity::Error
                        && entry.notice.message == "terminal exited with status 7")
            );
        } else {
            assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
            assert!(
                app.notices
                    .entries
                    .iter()
                    .any(|entry| entry.notice.severity == NoticeSeverity::Error
                        && entry.notice.message.contains("fixture fatal outcome"))
            );
        }
        assert!(
            !app.notices
                .entries
                .iter()
                .any(|entry| entry.notice.message.starts_with("Terminal input rejected"))
        );
    }
}

#[test]
fn input_notice_preserves_the_actual_live_app_original_and_next_input() {
    let (mut app, card, mut peer) = fixture();
    let proof = peer.handle.proof().unwrap();
    let continuity = terminal_continuity(app.card_state(card)).unwrap().clone();
    let recovery_attempts = app.terminal_recovery_attempts.clone();
    let uncertain = app.uncertain_terminal_proofs.clone();
    let mut requests = peer.observed.try_iter().collect::<Vec<_>>();
    peer.handle.input(b"accepted prefix".to_vec()).unwrap();
    loop {
        let request = peer.observed.recv_timeout(Duration::from_secs(3)).unwrap();
        let accepted =
            matches!(&request, ObservedInput::Input(bytes) if bytes == b"accepted prefix");
        requests.push(request);
        if accepted {
            break;
        }
    }
    let durable = Arc::clone(&app.durable);
    reset();
    const MESSAGE: &str = "Unsent input was canceled. Previously sent input was preserved.";
    apply(
        &mut app,
        card,
        &mut peer,
        Some(WireTerminalEvent::InputNotice {
            operation: "cancel terminal input".into(),
            message: MESSAGE.into(),
        }),
    );
    assert_eq!(counts(), (0, 0), "input notice changes only presentation");
    assert!(Arc::ptr_eq(&durable, &app.durable));
    assert_eq!(terminal_continuity(app.card_state(card)), Some(&continuity));
    assert_eq!(app.terminal_recovery_attempts, recovery_attempts);
    assert_eq!(app.uncertain_terminal_proofs, uncertain);
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Terminal(surface))
            if surface.terminal_id() == peer.handle.terminal_id()
                && surface.host_proof() == Some(proof.clone())
    ));
    assert!(ended_terminal(app.card_state(card)).is_none());
    assert!(app.notices.entries.iter().any(|entry| {
        entry.notice.severity == NoticeSeverity::Warning && entry.notice.message == MESSAGE
    }));
    // A different output generation and a new user interrupt still use the
    // original open connection; the accepted prefix must never be replayed.
    peer.sequence += 1;
    let generation = peer.sequence;
    let mut output = RenderSnapshot::empty();
    output.generation = generation;
    output.columns = 1;
    output.rows = 1;
    output.scroll.total = 1;
    output.scroll.len = 1;
    output.lines = vec![Arc::new(crate::terminal::RowSnapshot {
        cells: vec![crate::terminal::CellSnapshot {
            text: crate::terminal::CellText::new("Z"),
            ..Default::default()
        }],
        ..Default::default()
    })]
    .into();
    peer.send
        .send(ServerMessage::Snapshot {
            terminal_id: peer.handle.terminal_id(),
            snapshot: TerminalSnapshot::full(&output),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while peer.handle.generation() < generation {
        assert!(
            Instant::now() < deadline,
            "output after the nonfatal notice"
        );
        thread::yield_now();
    }
    drop(app.update(Message::TerminalWake { card, generation }));
    assert_eq!(peer.handle.snapshot().generation, generation);
    assert_eq!(peer.handle.snapshot().lines[0].cells[0].text.as_str(), "Z");
    peer.handle.input(vec![3]).unwrap();
    loop {
        let request = peer.observed.recv_timeout(Duration::from_secs(3)).unwrap();
        let interrupt = matches!(&request, ObservedInput::Input(bytes) if bytes == &[3]);
        requests.push(request);
        if interrupt {
            break;
        }
    }
    requests.extend(peer.observed.try_iter());
    assert_eq!(
        requests
            .iter()
            .filter(|request| matches!(request, ObservedInput::Connect))
            .count(),
        1,
        "the nonfatal notice must not reconnect the original attachment"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| matches!(request,
            ObservedInput::Input(bytes) if bytes == b"accepted prefix"))
            .count(),
        1,
        "never replay previously delivered input"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| matches!(request,
            ObservedInput::Input(bytes) if bytes == &[3]))
            .count(),
        1,
        "one new user interrupt, no automatic retry"
    );
    assert!(!peer.worker.as_ref().unwrap().is_finished());
    assert_eq!(peer.handle.proof(), Some(proof));
}

#[test]
fn late_input_notice_cannot_replace_a_fatal_or_completed_original() {
    for completed in [false, true] {
        let (mut app, card, mut peer) = fixture();
        let outcome = if completed {
            WireTerminalEvent::Completed {
                exit_code: 7,
                signal: None,
                intentional: false,
            }
        } else {
            // Even identical recovery wording is fatal when the trusted
            // producer classified it as Error. App never matches error text.
            WireTerminalEvent::Error {
                operation: "cancel terminal input".into(),
                message: "Unsent input was canceled. Previously sent input was preserved.".into(),
            }
        };
        peer.publish(Some(outcome));
        let generation = peer.publish(Some(WireTerminalEvent::InputNotice {
            operation: "cancel terminal input".into(),
            message: "late notice must not replace the original outcome".into(),
        }));
        drop(app.update(Message::TerminalWake { card, generation }));
        if completed {
            assert!(ended_terminal(app.card_state(card)).is_some());
            assert!(app.notices.entries.iter().any(|entry| {
                entry.notice.severity == NoticeSeverity::Error
                    && entry.notice.message == "terminal exited with status 7"
            }));
        } else {
            assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
            assert!(app.notices.entries.iter().any(|entry| {
                entry.notice.severity == NoticeSeverity::Error
                    && entry
                        .notice
                        .message
                        .contains("Previously sent input was preserved")
            }));
        }
        assert!(!app.notices.entries.iter().any(|entry| {
            entry
                .notice
                .message
                .contains("late notice must not replace")
        }));
    }
}

#[test]
fn local_relaunch_retires_old_unread_without_acknowledging_it_or_same_session_reattach() {
    use crate::agent::{Activity, ActivityState, Provider};

    for ended in [false, true] {
        let (mut app, card, mut peer) = fixture();
        app.refresh_ui.focused = false;
        let CardKind::Terminal(original) = app.card_state(card).unwrap().kind.clone() else {
            panic!("terminal fixture");
        };
        // A separate card's unread attribution must survive the relaunch.
        let mut other = app.card_state(card).unwrap().clone();
        other.id = CardId::new();
        let unrelated = other.id;
        if let CardKind::Terminal(session) = &mut other.kind {
            session.id = TerminalId::new();
        }
        Arc::make_mut(&mut app.durable).cards.push(other);
        app.sidebar_state.observe_card_update(unrelated);
        let notice = uuid::Uuid::new_v4();
        let journal = |id, source| crate::notifications::Snapshot {
            diagnostic: None,
            notices: vec![crate::notifications::Notice {
                id,
                delivered: true,
                acknowledged: false,
                window: None,
                event: crate::notifications::Event {
                    content: None,
                    target: crate::notifications::Target::Session(source),
                    source_session: Some(source),
                    provider: Some(Provider::Claude),
                    directory: Some(original.launch.working_directory.clone()),
                    destination: None,
                    evidence: crate::notifications::Evidence {
                        key: "local-relaunch-unread".into(),
                        kind: crate::notifications::Kind::Succeeded,
                    },
                },
            }],
        };
        let old_journal = journal(notice, original.id);
        drop(app.update_notifications(notification_ui::Action::Snapshot(old_journal.clone())));
        let waiting = Activity {
            provider: Provider::Claude,
            state: ActivityState::Waiting,
        };
        apply(
            &mut app,
            card,
            &mut peer,
            Some(WireTerminalEvent::AgentActivityChanged {
                activity: Some(waiting),
            }),
        );
        assert_eq!(app.sidebar_state.card_activity(card), Some(waiting));
        assert!(app.sidebar_state.has_card_update(card));
        if ended {
            // Use the real framed event and normal App reducer, rather than
            // fabricating an ended descriptor around a still-running actor.
            apply(
                &mut app,
                card,
                &mut peer,
                Some(WireTerminalEvent::Completed {
                    exit_code: 0,
                    signal: None,
                    intentional: false,
                }),
            );
            assert!(ended_terminal(app.card_state(card)).is_some());
            assert_eq!(app.sidebar_state.card_activity(card), None);
            assert!(app.sidebar_state.has_card_update(card));
        }
        let focused = app.workspace.focused();
        let durable_focused = app.durable.focused;
        // Dropping the returned task exercises the actual accepted relaunch
        // transition without executing any daemon/provider startup future.
        drop(app.update(Message::RetryCard(card)));
        let CardKind::Terminal(current) = app.card_state(card).unwrap().kind.clone() else {
            panic!("retry keeps the terminal pane");
        };
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert!(matches!(
            app.card_state(card).unwrap().residency,
            Residency::Hydrating { .. }
        ));
        assert_eq!(app.workspace.focused(), focused);
        assert_eq!(app.durable.focused, durable_focused);
        assert_eq!(current.launch, original.launch);
        assert!(app.sidebar_state.has_card_update(unrelated));
        assert!(!app.notification_ui.acknowledged_for_test(notice));
        assert!(!app.notification_ui.snapshot.notices[0].acknowledged);
        if !ended {
            assert_eq!(current.id, original.id, "same original actor reattachment");
            assert_eq!(app.sidebar_state.card_activity(card), Some(waiting));
            assert!(app.sidebar_state.has_card_update(card));
            continue;
        }
        assert_ne!(current.id, original.id);
        assert_eq!(current.predecessor, Some(original.id));
        assert_eq!(app.sidebar_state.card_activity(card), None);
        assert!(
            !app.sidebar_state.has_card_update(card),
            "old unread belongs to the old actor"
        );
        drop(app.update(Message::TerminalStarted {
            card,
            terminal: original.id,
            result: Err("late predecessor attachment".into()),
        }));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert!(matches!(&app.card_state(card).unwrap().kind,
            CardKind::Terminal(session) if session.id == current.id));
        drop(app.update_notifications(notification_ui::Action::Snapshot(old_journal)));
        assert!(
            !app.sidebar_state.has_card_update(card),
            "late old journal cannot rebind"
        );
        assert!(!app.notification_ui.acknowledged_for_test(notice));
        let fresh = uuid::Uuid::new_v4();
        drop(
            app.update_notifications(notification_ui::Action::Snapshot(journal(
                fresh, current.id,
            ))),
        );
        assert!(
            app.sidebar_state.has_card_update(card),
            "new actor's unread remains usable"
        );
        drop(app.update(Message::TerminalStarted {
            card,
            terminal: current.id,
            result: Err("current relaunch unavailable".into()),
        }));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
        assert!(
            app.sidebar_state.has_card_update(card),
            "failure is not reading fresh news"
        );
        assert!(app.sidebar_state.has_card_update(unrelated));
        assert!(!app.notification_ui.acknowledged_for_test(fresh));
        assert_eq!(app.workspace.focused(), focused);
    }
}

#[test]
fn refused_key_before_completion_keeps_the_actual_app_surface_and_exit_status() {
    for (message, retryable) in [
        ("the terminal worker has closed", true),
        ("terminal output has completed", false),
    ] {
        let (mut app, card, mut peer) = fixture();
        let proof = peer.handle.proof().unwrap();
        let continuity = terminal_continuity(app.card_state(card)).unwrap().clone();
        let uncertain = app.uncertain_terminal_proofs.clone();
        let recovery_attempts = app.terminal_recovery_attempts.clone();
        let mut observed = peer.observed.try_iter().collect::<Vec<_>>();
        *peer.reject_next_key.lock().unwrap() = Some(ProtocolError::new(
            ProtocolErrorCode::TerminalUnavailable,
            message,
            retryable,
        ));
        let key = crate::terminal::KeyInput {
            action: crate::terminal::KeyAction::Release,
            key: crate::terminal::TerminalKey::Enter,
            modifiers: crate::terminal::KeyModifiers::default(),
            text: None,
            unshifted_codepoint: None,
        };
        peer.handle.key(key.clone()).unwrap();
        loop {
            let request = peer.observed.recv_timeout(Duration::from_secs(3)).unwrap();
            let done = matches!(&request, ObservedInput::Key(actual) if *actual == key);
            observed.push(request);
            if done {
                break;
            }
        }
        let events = peer.handle.events();
        let deadline = Instant::now() + Duration::from_secs(3);
        while events.is_empty() {
            assert!(Instant::now() < deadline, "actual correlated key refusal");
            thread::sleep(Duration::from_millis(1));
        }
        // Force the refusal through App before any completion exists. A
        // generic Error here used to replace the retained terminal surface.
        let durable = Arc::clone(&app.durable);
        drop(app.update(Message::TerminalWake {
            card,
            generation: peer.handle.generation(),
        }));
        assert!(Arc::ptr_eq(&durable, &app.durable));
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Terminal(surface))
            if surface.terminal_id() == peer.handle.terminal_id() && surface.host_proof() == Some(proof.clone()))
        );
        assert_eq!(terminal_continuity(app.card_state(card)), Some(&continuity));
        assert!(
            ended_terminal(app.card_state(card)).is_none(),
            "a refusal is not an exit receipt"
        );
        assert_eq!(app.terminal_recovery_attempts, recovery_attempts);
        assert_eq!(app.uncertain_terminal_proofs, uncertain);
        assert!(
            app.notices
                .entries
                .iter()
                .any(|entry| entry.notice.severity == NoticeSeverity::Warning
                    && entry.notice.message.contains("this input was not sent"))
        );
        apply(
            &mut app,
            card,
            &mut peer,
            Some(WireTerminalEvent::Completed {
                exit_code: 23,
                signal: None,
                intentional: false,
            }),
        );
        assert!(matches!(
            &ended_terminal(app.card_state(card)).unwrap().continuity,
            TerminalContinuity::RelaunchAvailable { last_exit: Some(exit), .. }
                if exit.code == Some(23) && exit.signal.is_none()
        ));
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Terminal(surface))
            if surface.terminal_id() == peer.handle.terminal_id() && surface.host_proof() == Some(proof.clone()))
        );
        assert_eq!(
            app.card_state(card).unwrap().residency,
            Residency::Hibernated
        );
        assert!(
            app.notices
                .entries
                .iter()
                .any(|entry| entry.notice.severity == NoticeSeverity::Error
                    && entry.notice.message == "terminal exited with status 23")
        );
        observed.extend(peer.observed.try_iter());
        assert_eq!(
            observed
                .iter()
                .filter(|r| matches!(r, ObservedInput::Connect))
                .count(),
            1
        );
        assert_eq!(
            observed
                .iter()
                .filter(|r| matches!(r, ObservedInput::Key(actual) if *actual == key))
                .count(),
            1,
            "no reconnect or automatic replay of the refused physical key"
        );
        assert!(!peer.worker.as_ref().unwrap().is_finished());
    }
}

#[test]
fn worker_closed_wording_in_a_real_wire_error_remains_fatal() {
    let (mut app, card, mut peer) = fixture();
    apply(
        &mut app,
        card,
        &mut peer,
        Some(WireTerminalEvent::Error {
            operation: "send terminal key".into(),
            message: "the terminal worker has closed".into(),
        }),
    );
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
    assert!(ended_terminal(app.card_state(card)).is_none());
    assert!(app.notices.entries.iter().any(|entry| {
        entry.notice.severity == NoticeSeverity::Error
            && entry
                .notice
                .message
                .contains("the terminal worker has closed")
    }));
    assert!(
        !app.notices
            .entries
            .iter()
            .any(|entry| entry.notice.message.contains("Terminal input rejected"))
    );
}

#[test]
fn passive_folder_loads_rebind_cached_attention_without_navigation_or_losing_explicit_filing() {
    use crate::{
        agent::{Activity, ActivityState, Provider},
        model::{BootId, HostIncarnation, HostSessionId, TerminalId},
    };

    fn passive_load(app: &mut Cagents, directory: &Path) -> ProjectId {
        let loaded = load_project(
            directory,
            None,
            CancellationToken::default(),
            Some(ProjectKind::Folder),
        )
        .unwrap();
        assert!(loaded.worktrees.is_empty());
        let project = loaded.project.id;
        app.repository_generations.insert(project, 1);
        drop(app.update(Message::RepositoryLoaded {
            open_cards: false,
            project_id: project,
            generation: 1,
            result: Ok(Box::new(loaded)),
        }));
        project
    }

    for explicitly_filed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let control = root.join("control");
        let parent = root.join("parent");
        let nested = parent.join("nested");
        for path in [&control, &nested] {
            std::fs::create_dir_all(path).unwrap();
        }
        let mut app = super::tests::context_inspector_test_app(true);
        let control_card = app.workspace.focused().unwrap();
        let CardKind::Terminal(control_session) =
            &mut app.card_state_mut(control_card).unwrap().kind
        else {
            panic!("original control pane is a terminal")
        };
        control_session.launch.working_directory = control.clone();
        let mut session = control_session.clone();
        drop(app.select_worktree(control.clone(), false));
        app.refresh_ui.focused = false;
        let original_focus = app.workspace.focused();
        let original_strip = app.workspace.order().collect::<Vec<_>>();
        FOLDER_ROOT_REFRESHES.set(0);

        let parent_id = passive_load(&mut app, &parent);
        let parent_key = app.sidebar_model.worktree_at(&parent).unwrap();
        assert_eq!(parent_key.project_id, parent_id);
        assert!(app.known_worktree_roots.contains(&parent));
        assert_eq!(FOLDER_ROOT_REFRESHES.get(), 1);
        assert_eq!(app.active_workspace.as_ref(), Some(&control));
        assert_eq!(app.workspace.focused(), original_focus);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), original_strip);

        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("passive-folder-test"),
            boot_id: BootId::new("passive-folder-test"),
            host_incarnation: HostIncarnation::new("passive-folder-test"),
        };
        session.id = TerminalId::new();
        session.launch.working_directory = nested.clone();
        session.continuity = TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        let terminal = session.id;
        let mut peer = Peer::new(session.clone(), proof.clone());
        let mut descriptor =
            TerminalDescriptor::relaunchable(terminal, session.launch.clone(), None);
        descriptor.session = session;
        drop(app.update(Message::HostedTerminalsObserved(vec![descriptor])));
        let adopted = app
            .durable
            .cards
            .iter()
            .filter(|entry| matches!(&entry.kind, CardKind::Terminal(session) if session.id == terminal))
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        assert_eq!(adopted.len(), 1);
        let card = adopted[0];
        assert!(!app.durable.card_scopes.contains_key(&card));
        assert_eq!(
            app.card_worktree(app.card_state(card).unwrap()),
            Some(parent.as_path())
        );
        app.cards
            .insert(card, CardRuntime::Terminal(peer.handle.clone().into()));
        if explicitly_filed {
            // An established user filing is durable authority, unlike the
            // derived owner cache that a new Folder boundary must invalidate.
            Arc::make_mut(&mut app.durable)
                .card_scopes
                .insert(card, parent.clone());
        }
        let working = Activity {
            provider: Provider::Omp,
            state: ActivityState::Working,
        };
        apply(
            &mut app,
            card,
            &mut peer,
            Some(WireTerminalEvent::AgentActivityChanged {
                activity: Some(working),
            }),
        );
        apply(&mut app, card, &mut peer, Some(WireTerminalEvent::Bell));
        assert_eq!(
            app.sidebar_state
                .agent_activity(parent_key.project_id, parent_key.worktree_id),
            Some(ActivityState::Working)
        );
        assert!(
            app.sidebar_state
                .has_agent_update(parent_key.project_id, parent_key.worktree_id)
        );
        let session_before = match &app.card_state(card).unwrap().kind {
            CardKind::Terminal(session) => session.clone(),
            _ => panic!(),
        };
        let generation = peer.handle.generation();

        let nested_id = passive_load(&mut app, &nested);
        let nested_key = app.sidebar_model.worktree_at(&nested).unwrap();
        assert_eq!(nested_key.project_id, nested_id);
        assert!(app.known_worktree_roots.contains(&nested));
        assert_eq!(FOLDER_ROOT_REFRESHES.get(), 2);
        assert_eq!(app.active_workspace.as_ref(), Some(&control));
        assert_eq!(app.workspace.focused(), original_focus);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), original_strip);
        assert_eq!(app.sidebar_state.card_activity(card), Some(working));
        assert!(app.sidebar_state.has_card_update(card));
        assert_eq!(peer.handle.terminal_id(), terminal);
        assert_eq!(peer.handle.proof(), Some(proof.clone()));
        assert_eq!(
            peer.handle.generation(),
            generation,
            "no fresh terminal event authorizes reattribution"
        );
        match &app.card_state(card).unwrap().kind {
            CardKind::Terminal(session) => assert_eq!(*session, session_before),
            _ => panic!(),
        }
        assert_eq!(app.durable.cards.iter().filter(|entry| matches!(&entry.kind, CardKind::Terminal(session) if session.id == terminal)).count(), 1);
        let owner = if explicitly_filed { &parent } else { &nested };
        let owner_key = if explicitly_filed {
            &parent_key
        } else {
            &nested_key
        };
        let other_key = if explicitly_filed {
            &nested_key
        } else {
            &parent_key
        };
        assert_eq!(
            app.card_worktree(app.card_state(card).unwrap()),
            Some(owner.as_path())
        );
        assert_eq!(
            app.sidebar_state
                .agent_activity(owner_key.project_id, owner_key.worktree_id),
            Some(ActivityState::Working)
        );
        assert!(
            app.sidebar_state
                .has_agent_update(owner_key.project_id, owner_key.worktree_id)
        );
        assert_eq!(
            app.sidebar_state
                .agent_activity(other_key.project_id, other_key.worktree_id),
            None
        );
        assert!(
            !app.sidebar_state
                .has_agent_update(other_key.project_id, other_key.worktree_id)
        );
        assert_eq!(
            app.durable.card_scopes.get(&card),
            explicitly_filed.then_some(&parent)
        );

        // Repeated passive loads/ordinary layout and output events do not
        // repeat canonicalization or rescan all original card owners.
        passive_load(&mut app, &nested);
        drop(app.update(Message::WindowResized(1200.0, 800.0)));
        for _ in 0..4 {
            apply(&mut app, card, &mut peer, None);
        }
        assert_eq!(FOLDER_ROOT_REFRESHES.get(), 2);
        assert_eq!(app.sidebar_state.card_activity(card), Some(working));
        assert!(
            app.sidebar_state
                .has_agent_update(owner_key.project_id, owner_key.worktree_id)
        );
        let requests = peer.observed.try_iter().collect::<Vec<_>>();
        assert_eq!(
            requests
                .iter()
                .filter(|request| matches!(request, ObservedInput::Connect))
                .count(),
            1
        );
        assert!(
            requests
                .iter()
                .all(|request| matches!(request, ObservedInput::Connect)),
            "Folder adoption must not synthesize terminal input"
        );
    }
}
