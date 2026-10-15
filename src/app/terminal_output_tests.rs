//! Real hosted-client delivery into Application::update; no daemon or PTY.
use super::*;
use crate::terminal::{
    RenderSnapshot,
    ipc::{
        ClientEnvelope, ClientRequest, FrameCodec, FrameError, MAX_FRAME_BYTES, PROTOCOL_VERSION,
        ServerEnvelope, ServerMessage, ServerResponse, TerminalDescriptor, TerminalSnapshot,
        WireTerminalEvent, write_frame,
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
}
pub(super) fn record_projection() {
    PROJECTIONS.set(PROJECTIONS.get() + 1);
}
pub(super) fn record_protection() {
    PROTECTIONS.set(PROTECTIONS.get() + 1);
}
fn counts() -> (usize, usize) {
    (PROJECTIONS.get(), PROTECTIONS.get())
}
fn reset() {
    PROJECTIONS.set(0);
    PROTECTIONS.set(0);
}

struct Peer {
    _directory: tempfile::TempDir,
    handle: HostedTerminalHandle,
    send: crossbeam_channel::Sender<ServerMessage>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    sequence: u64,
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
                let response = if !connected {
                    assert!(matches!(request.request, ClientRequest::Connect { .. }));
                    connected = true;
                    ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![descriptor.clone()],
                    }
                } else {
                    ServerResponse::Accepted
                };
                if write_frame(
                    &mut stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request.request_id),
                        proof: proof.clone(),
                        message: ServerMessage::Response { response },
                    },
                )
                .is_err()
                {
                    return;
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

#[test]
fn stable_activity_and_unread_move_on_structural_ownership_changes() {
    use crate::{
        agent::{Activity, ActivityState, Provider},
        model::{ChannelId, ProjectChannel, WorktreeId},
        repository::RepositoryId,
        sidebar::{SidebarProject, SidebarWorktree},
        workspace_model::{ModelChannel, ModelProject, WorkspaceModel},
    };
    let (mut app, card, mut peer) = fixture();
    let activity = Activity {
        provider: Provider::Claude,
        state: ActivityState::Working,
    };
    apply(
        &mut app,
        card,
        &mut peer,
        Some(WireTerminalEvent::AgentActivityChanged {
            activity: Some(activity),
        }),
    );
    app.sidebar_state.observe_card_update(card);
    let root = card_directory(app.card_state(card).unwrap())
        .unwrap()
        .to_path_buf();
    let project = ProjectId::new();
    let worktree = WorktreeId::new();
    app.sidebar_model.projects.clear();
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "Output fixture".into(),
        root.clone(),
        vec![SidebarWorktree::new(
            worktree,
            root.clone(),
            None,
            false,
            false,
            None,
            None,
            None,
        )],
        vec![],
    ));
    let mut model = WorkspaceModel {
        identity: uuid::Uuid::new_v4(),
        sequence: 1,
        projects: vec![ModelProject {
            repository: root.clone(),
            name: "Output fixture".into(),
            kind: crate::model::ProjectKind::Git,
            ordinal: 0,
        }],
        ..WorkspaceModel::default()
    };
    drop(app.update(Message::WorkspaceModelObserved(
        workspace_model_sync::ModelUpdate::Read(model.clone()),
    )));
    assert_eq!(
        app.sidebar_state.agent_activity(project, worktree),
        Some(ActivityState::Working)
    );
    assert!(app.sidebar_state.has_agent_update(project, worktree));
    assert_eq!(app.sidebar_state.card_activity(card), Some(activity));
    reset();
    apply(&mut app, card, &mut peer, None);
    assert_eq!(counts(), (0, 0));
    assert!(app.sidebar_state.has_agent_update(project, worktree));
    let channel = ChannelId::new();
    Arc::make_mut(&mut app.durable)
        .channels
        .push(ProjectChannel {
            id: channel,
            project_id: project,
            title: "Moved".into(),
            title_source: Default::default(),
            ordinal: 0,
            shared: true,
            cards: vec![card],
            focused: Some(card),
        });
    model.sequence += 1;
    model.channels.push(ModelChannel {
        id: channel,
        repository: root,
        title: "Moved".into(),
        title_source: Default::default(),
        ordinal: 0,
    });
    drop(app.update(Message::WorkspaceModelObserved(
        workspace_model_sync::ModelUpdate::Read(model),
    )));
    assert_eq!(app.sidebar_state.agent_activity(project, worktree), None);
    assert!(!app.sidebar_state.has_agent_update(project, worktree));
    assert!(
        app.sidebar_state.has_card_update(card),
        "moving scope is not acknowledgement"
    );
    assert_eq!(app.sidebar_state.card_activity(card), Some(activity));
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
            vec![row(second_row, root.clone()), row(nested_row, nested.clone())],
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
    app.refresh_sidebar_worktree_panes();
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
    app.refresh_sidebar_worktree_panes();
    assert_eq!(
        app.sidebar_state.agent_activity(first, folder.worktree_id),
        None
    );
    assert_eq!(
        app.sidebar_state.agent_activity(second, nested_row),
        Some(ActivityState::Working)
    );
    assert!(app.sidebar_state.has_agent_update(second, nested_row));
    assert!(!app.sidebar_state.worktree_has_panes(first, first_row));
    assert!(app.sidebar_state.worktree_has_panes(second, nested_row));
    app.worktree_owners.remove(&card);
    Arc::make_mut(&mut app.durable).card_scopes.remove(&card);
    app.known_worktree_roots.clear();
    for project in &mut app.sidebar_model.projects {
        project.worktrees.clear();
    }
    if let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind {
        session.launch.working_directory = root.join("unregistered-child");
    }
    app.refresh_sidebar_worktree_panes();
    assert_eq!(
        app.sidebar_state.agent_activity(first, folder.worktree_id),
        None,
        "an exact folder key is not a newly discovered ancestor root"
    );
    assert!(app.sidebar_state.has_card_update(card));
    assert_eq!(app.sidebar_state.card_activity(card), Some(working));
}

#[test]
fn duplicate_full_reads_skip_projection_but_view_acknowledgements_do_not() {
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
    assert_eq!(counts(), (0, 0));
    assert!(Arc::ptr_eq(&durable, &app.durable));
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
    assert_eq!(counts(), (0, 0));
}
