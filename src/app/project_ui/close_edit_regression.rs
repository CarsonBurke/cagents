//! Late project-close editor fences through the real asynchronous cleanup path.
//! A private framed peer gates cleanup and owns no providers or real daemon.
//! Mounted widget events are headless component evidence, not native acceptance.
use super::*;
use crate::terminal::ipc::{
    ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage, ServerResponse,
    read_frame, write_frame,
};
use crate::workspace_model::{ModelEdit, ModelProject, WorkspaceModel};
use cosmic::iced::futures::StreamExt;
use std::os::unix::net::{UnixListener, UnixStream};

const CHILD: &str = "ADE_PROJECT_CLOSE_TEST_CHILD";
const WAIT: Duration = Duration::from_secs(5);
const CHILD_TIMEOUT: Duration = Duration::from_secs(40);
const BASELINE: &str = "baseline\n";

#[derive(Clone, Copy)]
enum Case {
    EditDuringCleanup,
    NoEdit,
    ReopenBeforeCompletion,
    SaveDuringCleanup,
    DurabilityDuringCleanup,
    RecoveryDuringCleanup,
}

impl Case {
    fn resumes(self) -> bool {
        matches!(
            self,
            Self::SaveDuringCleanup | Self::DurabilityDuringCleanup | Self::RecoveryDuringCleanup
        )
    }
}

// Own only the exact self-spawned test child. Reap it even if waiting or an
// assertion fails; never signal a process group, daemon, or unrelated peer.
struct TestChild(std::process::Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

// Match the repository's existing self-spawned test pattern. Setting XDG roots
// on a new process avoids global environment mutation and private-state reads.
fn isolated(name: &str, case: Case) {
    if std::env::var(CHILD).as_deref() == Ok(name) {
        // This helper creates and blocks on its own Tokio runtime. Finish it
        // before entering the runtime which drives the actual cleanup task.
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(case, renderer));
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    for leaf in ["config", "data", "state", "cache", "runtime"] {
        use std::os::unix::fs::PermissionsExt;
        let path = directory.path().join(leaf);
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let stdout = directory.path().join("stdout");
    let stderr = directory.path().join("stderr");
    // Files avoid pipe backpressure. The deadline bounds all child startup,
    // renderer construction, fixture threads, cleanup and assertions together.
    let deadline = Instant::now() + CHILD_TIMEOUT;
    let mut child = TestChild(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD, name)
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .env("XDG_DATA_HOME", directory.path().join("data"))
            .env("XDG_STATE_HOME", directory.path().join("state"))
            .env("XDG_CACHE_HOME", directory.path().join("cache"))
            .env("XDG_RUNTIME_DIR", directory.path().join("runtime"))
            .stdin(std::process::Stdio::null())
            .stdout(std::fs::File::create(&stdout).unwrap())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    let mut timed_out = false;
    let status = loop {
        match child.0.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                timed_out = true;
                // Only this owned child is signalled. A concurrent normal exit
                // is harmless; wait still reaps that same child.
                let _ = child.0.kill();
                break child.0.wait().expect("reap the timed-out test child");
            }
            Err(error) => panic!("could not poll isolated test child: {error}"),
        }
    };
    drop(child);
    let stdout = std::fs::read_to_string(stdout).unwrap();
    let stderr = std::fs::read_to_string(stderr).unwrap();
    assert!(
        !timed_out && status.success(),
        "isolated test {name} finished with {status}; timed_out={timed_out}\n{stdout}\n{stderr}",
    );
    assert!(stdout.contains(&format!("test {name} ... ok")));
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"));
    eprintln!("isolated child {name}:\n{stdout}{stderr}");
}

#[test]
fn peer_close_retains_widget_edits_entered_during_cleanup() {
    isolated(
        "app::project_ui::close_edit_regression::peer_close_retains_widget_edits_entered_during_cleanup",
        Case::EditDuringCleanup,
    );
}

#[test]
fn peer_close_without_an_intervening_edit_still_retires_the_project() {
    isolated(
        "app::project_ui::close_edit_regression::peer_close_without_an_intervening_edit_still_retires_the_project",
        Case::NoEdit,
    );
}

#[test]
fn successful_old_cleanup_cannot_retire_a_project_reopened_by_the_peer() {
    isolated(
        "app::project_ui::close_edit_regression::successful_old_cleanup_cannot_retire_a_project_reopened_by_the_peer",
        Case::ReopenBeforeCompletion,
    );
}

#[test]
fn peer_close_waits_for_a_late_unpolled_save_then_recensuses_after_resolution() {
    isolated(
        "app::project_ui::close_edit_regression::peer_close_waits_for_a_late_unpolled_save_then_recensuses_after_resolution",
        Case::SaveDuringCleanup,
    );
}

#[test]
fn peer_close_retains_a_late_clean_durability_warning_until_reload() {
    isolated(
        "app::project_ui::close_edit_regression::peer_close_retains_a_late_clean_durability_warning_until_reload",
        Case::DurabilityDuringCleanup,
    );
}

#[test]
fn peer_close_retains_a_late_clean_recovery_owner_until_resolution() {
    isolated(
        "app::project_ui::close_edit_regression::peer_close_retains_a_late_clean_recovery_owner_until_resolution",
        Case::RecoveryDuringCleanup,
    );
}

fn editor(app: &Cagents, card: CardId) -> &RepositoryRuntime {
    let Some(CardRuntime::Repository(editor)) = app.cards.get(&card) else {
        panic!("the original editor must remain installed");
    };
    editor
}

async fn repository_output(task: iced::Task<RepositoryAction>) -> RepositoryAction {
    let mut stream = iced::runtime::task::into_stream(task).expect("a file worker was scheduled");
    while let Some(action) = stream.next().await {
        if let iced::runtime::Action::Output(action) = action {
            return action;
        }
    }
    panic!("the file worker must emit its real result");
}

async fn close_output(task: Task<Message>) -> Message {
    let mut stream = iced::runtime::task::into_stream(task).expect("a fresh cleanup was scheduled");
    while let Some(action) = stream.next().await {
        if let iced::runtime::Action::Output(cosmic::Action::App(
            message @ Message::ProjectUi(Action::ProjectSessionsClosed(..)),
        )) = action
        {
            return message;
        }
    }
    panic!("fresh cleanup must emit its own completion");
}

fn respond(stream: &mut UnixStream, request: ClientEnvelope, response: ServerResponse) {
    write_frame(
        stream,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request.request_id),
            proof: proof(),
            message: ServerMessage::Response { response },
        },
    )
    .unwrap();
}

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("close-edit-fixture"),
        boot_id: crate::model::BootId::new("close-edit-boot"),
        host_incarnation: crate::model::HostIncarnation::new("close-edit-host"),
    }
}

fn handshake(stream: &mut UnixStream) {
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.set_write_timeout(Some(WAIT)).unwrap();
    let request: ClientEnvelope = read_frame(stream).unwrap().unwrap();
    assert!(matches!(&request.request, ClientRequest::Connect { .. }));
    respond(
        stream,
        request,
        ServerResponse::Connected {
            proof: proof(),
            resumed: Vec::new(),
        },
    );
}

// Focus and type through the actual RepositoryRuntime view. The action passed
// to App::update below is emitted by its native TextEditor, not manufactured.
fn type_in_mounted_editor(
    app: &Cagents,
    card: CardId,
    renderer: &cosmic::Renderer,
) -> RepositoryAction {
    use cosmic::iced::advanced::{
        Layout, Shell, clipboard, layout, mouse,
        widget::{Tree, operation},
    };
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        panic!("the clean editor must remain mounted during cleanup");
    };
    let mut element = repository_widget::view(card, runtime);
    let mut tree = Tree::new(element.as_widget());
    let viewport = iced::Rectangle::with_size(iced::Size::new(1000.0, 600.0));
    let node = element.as_widget_mut().layout(
        &mut tree,
        renderer,
        &layout::Limits::new(iced::Size::ZERO, viewport.size()),
    );
    let mut focus = operation::focusable::focus::<()>(repository_widget::editor_focus_id(card));
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), renderer, &mut focus);
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    let event = iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
        key: iced::keyboard::Key::Character("x".into()),
        modified_key: iced::keyboard::Key::Character("x".into()),
        physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::KeyX),
        location: iced::keyboard::Location::Standard,
        modifiers: iced::keyboard::Modifiers::empty(),
        text: Some("x".into()),
        repeat: false,
    });
    element.as_widget_mut().update(
        &mut tree,
        &event,
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard::Null,
        &mut shell,
        &viewport,
    );
    drop(shell);
    messages.into_iter().find(|action| matches!(action,
        RepositoryAction::EditorAction { card: target, action: repository_widget::EditorAction::Edit(_), .. }
            if *target == card
    )).expect("the focused native editor must emit the actual edit")
}

async fn run(case: Case, renderer: cosmic::Renderer) {
    // All path-dependent helpers below must remain inside the child's XDG roots.
    let data = PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap());
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    std::fs::create_dir_all(&runtime).unwrap();
    assert!(
        crate::instance::window_database_paths()
            .unwrap()
            .iter()
            .all(|path| path.starts_with(&data))
    );
    let socket = crate::terminal::ipc::daemon_socket_path().unwrap();
    assert!(socket.starts_with(&runtime));
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("checkout");
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join("notes.txt"), BASELINE).unwrap();
    let project = ProjectId::new();
    let card = CardId::new();
    let registry = uuid::Uuid::new_v4();
    let model = WorkspaceModel {
        identity: registry,
        sequence: 1,
        projects: vec![ModelProject {
            repository: root.clone(),
            name: "Fixture folder".into(),
            kind: ProjectKind::Folder,
            ordinal: 7,
        }],
        ..Default::default()
    };
    let close = ModelEdit::CloseProject {
        repository: root.clone(),
    };
    let mut closed_model = model.clone();
    closed_model.fold(2, &close);
    let (edits, receive_edits) = std::sync::mpsc::channel::<ModelEdit>();
    let (cleanup_entered, entered) = tokio::sync::oneshot::channel();
    let (release_cleanup, cleanup_release) = std::sync::mpsc::channel();
    let cleanup_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let completed_cleanups = cleanup_count.clone();
    let served_model = model.clone();
    let server = std::thread::spawn(move || {
        let (mut watcher, _) = listener.accept().unwrap();
        handshake(&mut watcher);
        let watch: ClientEnvelope = read_frame(&mut watcher).unwrap().unwrap();
        assert!(matches!(&watch.request, ClientRequest::WatchWorkspaceModel));
        respond(
            &mut watcher,
            watch,
            ServerResponse::WorkspaceModel {
                model: served_model,
            },
        );
        let watcher = std::thread::spawn(move || {
            for (index, edit) in receive_edits.into_iter().enumerate() {
                write_frame(
                    &mut watcher,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: None,
                        proof: proof(),
                        message: ServerMessage::WorkspaceModelEdited {
                            sequence: index as u64 + 2,
                            edit,
                        },
                    },
                )
                .unwrap();
            }
        });
        let mut cleanup_entered = Some(cleanup_entered);
        for index in 0..if case.resumes() { 2 } else { 1 } {
            let (mut cleanup, _) = listener.accept().unwrap();
            handshake(&mut cleanup);
            let read: ClientEnvelope = read_frame(&mut cleanup).unwrap().unwrap();
            assert!(matches!(&read.request, ClientRequest::ReadWorkspaceModel));
            respond(
                &mut cleanup,
                read,
                ServerResponse::WorkspaceModel {
                    model: closed_model.clone(),
                },
            );
            let chats: ClientEnvelope = read_frame(&mut cleanup).unwrap().unwrap();
            assert!(matches!(&chats.request, ClientRequest::ChatList));
            respond(
                &mut cleanup,
                chats,
                ServerResponse::ChatListed {
                    sessions: Vec::new(),
                },
            );
            let terminals: ClientEnvelope = read_frame(&mut cleanup).unwrap().unwrap();
            assert!(matches!(&terminals.request, ClientRequest::List));
            if index == 0 {
                cleanup_entered.take().unwrap().send(()).unwrap();
                cleanup_release
                    .recv_timeout(WAIT)
                    .expect("release the real inventory response");
            }
            completed_cleanups.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            respond(
                &mut cleanup,
                terminals,
                ServerResponse::Listed {
                    terminals: Vec::new(),
                },
            );
        }
        watcher.join().unwrap();
    });

    let mut app = super::super::tests::context_inspector_test_app(false);
    app.context_inspector_open = false;
    app.settings.editor_auto_save = false;
    app.settings.language_servers.clear();
    app.lsp = LspManager::new(Vec::new());
    app.sidebar_model.projects.push(
        SidebarProject::new(
            project,
            RepositoryId::new(),
            "Fixture folder".into(),
            root.clone(),
            Vec::new(),
            Vec::new(),
        )
        .with_kind(ProjectKind::Folder),
    );
    let descriptor = RepositoryView {
        project,
        repository: root.clone(),
        worktree: root.clone(),
        selected_file: Some("notes.txt".into()),
    };
    // Load the real file so later Save owns its original read root. Every
    // edit below is emitted by the mounted editor; no dirty flag is patched.
    let mut runtime = RepositoryRuntime::new_directory(descriptor.clone());
    runtime.set_auto_save(false);
    let load = runtime.initial_load(card);
    let loaded = tokio::time::timeout(WAIT, repository_output(load))
        .await
        .unwrap();
    let RepositoryAction::FileLoadedAt { identity, .. } = &loaded else {
        panic!("the selected file must load through retained read");
    };
    let identity = identity.clone();
    drop(runtime.update(loaded));
    assert_eq!(runtime.editor_text().as_deref(), Some(BASELINE));
    let durable = Arc::make_mut(&mut app.durable);
    durable.workspace_model_identity = Some(registry);
    durable.cards.push(CardState {
        id: card,
        generation: 0,
        title: "notes.txt".into(),
        width_fraction: 1.0,
        existence: Existence::Open,
        exposure: Exposure::Visible,
        residency: Residency::Hot,
        kind: CardKind::Repository(descriptor),
    });
    durable.focused = Some(card);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.active_workspace = Some(root.clone());
    app.known_worktree_roots.insert(root.clone());
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));
    app.lifecycle
        .register(card, Exposure::Visible, Residency::Hot, Instant::now());
    app.workspace_model = model;
    assert!(!app.workspace_inputs_blocked());
    assert!(!app.project_has_unsaved_editors(project));

    let (updates, observed) = async_channel::unbounded();
    super::super::workspace_model_sync::observe_workspace_model(updates);
    let connected = tokio::time::timeout(WAIT, observed.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        connected,
        super::super::workspace_model_sync::ModelUpdate::Connected(_)
    ));
    drop(app.update(Message::WorkspaceModelObserved(connected)));
    edits.send(close).unwrap();
    let closed = tokio::time::timeout(WAIT, observed.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        &closed,
        super::super::workspace_model_sync::ModelUpdate::Edited {
            edit: ModelEdit::CloseProject { .. },
            ..
        }
    ));
    let task = app.update(Message::WorkspaceModelObserved(closed));
    let generation = app.project_ui.close_generation[&project];
    assert!(app.project_ui.close_in_flight.contains(&project));
    assert!(app.cards.contains_key(&card));
    assert!(!app.workspace_inputs_blocked());
    let mut stream =
        iced::runtime::task::into_stream(task).expect("the peer close starts real cleanup");
    let callback = async {
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(cosmic::Action::App(
                message @ Message::ProjectUi(Action::ProjectSessionsClosed(..)),
            )) = action
            {
                return message;
            }
        }
        panic!("real cleanup must emit its own completion");
    };
    tokio::pin!(callback);
    tokio::select! {
        value = entered => value.expect("worker reached the gated final inventory"),
        _ = &mut callback => panic!("cleanup finished before its inventory was released"),
        _ = tokio::time::sleep(WAIT) => panic!("cleanup never reached the fixture socket"),
    }

    let mut edited_text = None;
    let mut pending_save = None;
    let mut recovery = None;
    match case {
        Case::EditDuringCleanup | Case::SaveDuringCleanup | Case::DurabilityDuringCleanup => {
            let action = type_in_mounted_editor(&app, card, &renderer);
            drop(app.update(Message::Repository(action)));
            let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
                panic!("editor");
            };
            let text = runtime.editor_text().unwrap();
            assert_ne!(text, BASELINE);
            assert!(text.contains('x'));
            assert!(runtime.is_dirty());
            assert!(!runtime.is_saving());
            assert!(app.project_ui.close_in_flight.contains(&project));
            assert_eq!(app.project_ui.close_generation[&project], generation);
            edited_text = Some(text);
            if matches!(case, Case::SaveDuringCleanup) {
                // Admission marks the save active before its lazy task runs.
                let save = app.update(Message::Repository(RepositoryAction::Save {
                    card,
                    overwrite: false,
                }));
                assert!(editor(&app, card).is_saving());
                // Returning to the old disk bytes must not hide the pending
                // write: is_dirty() alone would miss this late barrier.
                drop(app.update(Message::Repository(RepositoryAction::Undo { card })));
                assert!(!editor(&app, card).is_dirty());
                assert!(editor(&app, card).is_saving());
                assert_eq!(editor(&app, card).editor_text().as_deref(), Some(BASELINE));
                pending_save = Some(save);
            } else if matches!(case, Case::DurabilityDuringCleanup) {
                assert!(app.prepare_editor_save(card));
                let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) else {
                    unreachable!();
                };
                // Existing failure hook performs the real save and injects
                // only source-directory fsync failure, with no recovery owner.
                let save = runtime.request_save_with_sync_warning_for_test(card);
                let saved = tokio::time::timeout(WAIT, repository_output(save))
                    .await
                    .unwrap();
                assert!(matches!(&saved,
                    RepositoryAction::RetainedSaveFinished { result: Ok(outcome), .. }
                        if outcome.recovery_authority().is_none()));
                drop(app.update(Message::Repository(saved)));
                assert!(!editor(&app, card).is_dirty());
                assert!(!editor(&app, card).is_saving());
                assert!(!editor(&app, card).has_editor_recovery());
                assert!(editor(&app, card).has_durability_warning());
            }
        }
        Case::RecoveryDuringCleanup => {
            // A bounded synthetic producer tests clean recovery custody
            // independently of dirty text, save tickets and durability. It
            // performs no filesystem mutation and grants no process authority.
            let authority = crate::recovery_authority::RecoverySlot::reserve()
                .unwrap()
                .retain("close-editor recovery fixture".into(), || Ok(()));
            drop(app.update(Message::Repository(
                RepositoryAction::RetainedSaveFinished {
                    identity,
                    ticket: 0,
                    revision: 0,
                    saved_text: BASELINE.into(),
                    result: Err(crate::editor::retained_save::RetainedSaveError::Recovery {
                        message: "fixture cleanup remains pending".into(),
                        authority: authority.clone(),
                    }),
                },
            )));
            assert!(editor(&app, card).has_editor_recovery());
            assert!(!editor(&app, card).is_dirty());
            assert!(!editor(&app, card).is_saving());
            assert!(!editor(&app, card).has_durability_warning());
            recovery = Some(authority);
        }
        Case::ReopenBeforeCompletion => {
            edits
                .send(ModelEdit::ReopenProject {
                    repository: root.clone(),
                    name: "Fixture folder".into(),
                    kind: ProjectKind::Folder,
                })
                .unwrap();
            let reopened = tokio::time::timeout(WAIT, observed.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                &reopened,
                super::super::workspace_model_sync::ModelUpdate::Edited {
                    edit: ModelEdit::ReopenProject { .. },
                    ..
                }
            ));
            drop(app.update(Message::WorkspaceModelObserved(reopened)));
            assert!(!app.project_ui.close_generation.contains_key(&project));
        }
        Case::NoEdit => {}
    }
    release_cleanup.send(()).unwrap();
    let message = tokio::time::timeout(WAIT, &mut callback).await.unwrap();
    assert!(
        matches!(&message, Message::ProjectUi(Action::ProjectSessionsClosed(
        id, completed_generation, Ok(_)
    )) if *id == project && *completed_generation == generation)
    );
    // Feed the unmodified message produced by the actual close worker.
    drop(app.update(message));
    assert_eq!(cleanup_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    match case {
        Case::EditDuringCleanup
        | Case::SaveDuringCleanup
        | Case::DurabilityDuringCleanup
        | Case::RecoveryDuringCleanup => {
            assert!(editor(&app, card).blocks_navigation());
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(app.sidebar_model.project(project).is_some());
            assert!(app.workspace.card(card).is_some());
            assert_eq!(app.workspace.focused(), Some(card));
            assert_eq!(
                app.project_ui.close_generation.get(&project),
                Some(&generation)
            );
            assert_eq!(app.project_ui.closing.get(&project), Some(&root));
            assert!(app.project_ui.closed.contains(&root));
            assert!(!app.project_ui.close_in_flight.contains(&project));
            assert!(!app.project_ui.close_failed.contains(&project));
            assert!(!app.workspace_inputs_blocked());
            assert!(
                app.notices
                    .entries
                    .iter()
                    .any(|entry| entry.notice.message.contains("resolve file recovery"))
            );
            if matches!(case, Case::SaveDuringCleanup) {
                assert_eq!(editor(&app, card).editor_text().as_deref(), Some(BASELINE));
                assert!(editor(&app, card).is_saving());
            } else if let Some(text) = &edited_text {
                assert_eq!(editor(&app, card).editor_text().as_ref(), Some(text));
            } else {
                assert_eq!(editor(&app, card).editor_text().as_deref(), Some(BASELINE));
            }
        }
        Case::NoEdit => {
            assert!(!app.cards.contains_key(&card));
            assert!(!app.card_state(card).unwrap().existence.is_open());
            assert!(app.sidebar_model.project(project).is_none());
        }
        Case::ReopenBeforeCompletion => {
            assert_eq!(editor(&app, card).editor_text().as_deref(), Some(BASELINE));
            assert!(!editor(&app, card).is_dirty());
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(app.sidebar_model.project(project).is_some());
        }
    }

    if case.resumes() {
        let resume = if let Some(save) = pending_save {
            let mut stream = iced::runtime::task::into_stream(save).unwrap();
            let saved = tokio::time::timeout(WAIT, async {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(
                        message @ Message::Repository(RepositoryAction::RetainedSaveFinished {
                            ..
                        }),
                    )) = action
                    {
                        return message;
                    }
                }
                panic!("the held save must emit its real completion");
            })
            .await
            .unwrap();
            drop(app.update(saved));
            assert!(!editor(&app, card).is_saving());
            assert!(editor(&app, card).is_dirty());
            // The earlier Undo is still visible. Explicit Discard now adopts
            // the completed save's bytes, rather than throwing that write away.
            app.update(Message::Repository(RepositoryAction::DiscardEdits { card }))
        } else {
            if let Some(authority) = recovery {
                tokio::task::spawn_blocking(move || authority.retry())
                    .await
                    .unwrap()
                    .unwrap();
            }
            let reload = app.update(Message::Repository(RepositoryAction::Reload { card }));
            // Reload keeps the old document fenced until its actual read lands.
            assert!(editor(&app, card).blocks_navigation());
            assert!(!app.project_ui.close_in_flight.contains(&project));
            let mut stream = iced::runtime::task::into_stream(reload).unwrap();
            let loaded = tokio::time::timeout(WAIT, async {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(
                        message @ Message::Repository(RepositoryAction::FileLoadedAt { .. }),
                    )) = action
                    {
                        return message;
                    }
                }
                panic!("Reload must emit its actual read result");
            })
            .await
            .unwrap();
            app.update(loaded)
        };
        assert!(!editor(&app, card).blocks_navigation());
        assert!(app.project_ui.close_in_flight.contains(&project));
        assert_eq!(
            app.project_ui.close_generation.get(&project),
            Some(&generation)
        );
        let completed = tokio::time::timeout(WAIT, close_output(resume))
            .await
            .unwrap();
        assert!(matches!(&completed,
            Message::ProjectUi(Action::ProjectSessionsClosed(id, same_generation, Ok(_)))
                if *id == project && *same_generation == generation));
        drop(app.update(completed));
        assert_eq!(
            cleanup_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "resolution must acquire fresh ownership/inventory, not reuse the first closure"
        );
        assert!(!app.cards.contains_key(&card));
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert!(app.sidebar_model.project(project).is_none());
        assert!(!app.project_ui.close_generation.contains_key(&project));
    }
    drop(edits);
    drop(observed);
    server.join().unwrap();
    let expected = if matches!(
        case,
        Case::SaveDuringCleanup | Case::DurabilityDuringCleanup
    ) {
        edited_text.as_deref().unwrap()
    } else {
        BASELINE
    };
    assert_eq!(
        std::fs::read_to_string(root.join("notes.txt")).unwrap(),
        expected
    );
}
