// Framed client/UI custody evidence. Peers certify no Host authorization or
// provider process; real helper authority is covered in HOST resolution tests.
fn custody_message(task: Task<Message>) -> Option<Message> {
    use crate::app::chat_ui::Action as ChatAction;
    let mut stream = cosmic::iced::runtime::task::into_stream(task)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            tokio::time::timeout(Duration::from_secs(10), async move {
                while let Some(action) = stream.next().await {
                    if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                        message @ Message::ChatUi(
                            ChatAction::StartCheckpointed(..)
                            | ChatAction::StartOwnerResolved(..)
                            | ChatAction::StartOwnerCheckpointed(..)
                            | ChatAction::StartCleanupFinished(..)
                            | ChatAction::StartCustodyReleased(..),
                        ),
                    )) = action
                    {
                        return Some(message);
                    }
                }
                None
            })
            .await
            .expect("bounded real custody task")
        })
}

pub(in crate::app) fn drain_custody_fixture(app: &mut Cagents, task: Task<Message>) {
    drain_custody(app, task);
}

fn drain_custody(app: &mut Cagents, mut task: Task<Message>) {
    for _ in 0..8 {
        let Some(message) = custody_message(task) else {
            return;
        };
        task = app.update(message);
    }
    panic!("custody must settle without polling/replay");
}

fn custody_app(name: &str) -> Cagents {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let registry = uuid::Uuid::new_v4();
    app.workspace_model.identity = registry;
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(registry);
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let (writer, _) = crate::session::SessionRuntime::open(
        root.join(format!("{name}.sqlite3")),
        crate::model::BootId::new("custody-test"),
    )
    .unwrap();
    app.session = Some(writer);
    app
}

fn synthetic_owner(raw: &crate::chat::Session) -> crate::chat::Session {
    let mut owner = raw.clone();
    owner.proof = Some(proof());
    owner.native_continuity = crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
    owner
}

fn start_through_checkpoint(
    app: &mut Cagents,
    target: crate::app::chat_ui::Target,
    source: crate::chat::Session,
    operation: uuid::Uuid,
) -> (
    Connection,
    ClientEnvelope,
    JoinHandle<Result<crate::chat::client::ClientHandle, crate::chat::StartFailure>>,
) {
    use crate::chat::client::{ClientHandle, Wake};
    let (stream, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let (events, receive) = std::sync::mpsc::channel();
    let raw = source.clone();
    let worker = thread::spawn(move || {
        ClientHandle::fixture_guarded_start(stream, source, operation, move |wake| {
            let _ = events.send(wake);
        })
    });
    let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
    assert!(matches!(
        hello.request,
        ClientRequest::Connect {
            expected_host: None,
            ..
        }
    ));
    let mut connection = Connection(peer);
    connection.response(
        &hello,
        ServerResponse::Connected {
            proof: proof(),
            resumed: vec![],
        },
    );
    let wake = receive.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(&wake, Wake::StartPrepared(actual, destination, _) if *actual == operation && destination == &proof())
    );
    let task = app.update(Message::ChatUi(crate::app::chat_ui::Action::Wake(
        target, wake,
    )));
    // The actual writer ACK must precede ChatStart on this same socket.
    let ack = custody_message(task).expect("actual destination checkpoint");
    let stored = app
        .session
        .as_ref()
        .unwrap()
        .operation_store()
        .load_workspace_manifest(app.durable.id)
        .unwrap()
        .unwrap();
    let entry = stored
        .native_start_custody
        .iter()
        .find(|entry| entry.operation == operation)
        .unwrap();
    assert_eq!(entry.source, raw);
    assert!(entry.source.proof.is_none());
    assert_eq!(entry.destination, Some(proof()));
    assert!(entry.observed.is_none());
    drop(app.update(ack));
    let start = connection.next();
    assert!(
        matches!(&start.request, ClientRequest::ChatStart { session, source: None, terminate_source: false, .. } if session == &raw)
    );
    (connection, start, worker)
}

fn actual_start_error(
    app: &mut Cagents,
    target: crate::app::chat_ui::Target,
    source: crate::chat::Session,
    operation: uuid::Uuid,
) -> crate::chat::StartFailure {
    let (mut connection, start, worker) = start_through_checkpoint(app, target, source, operation);
    connection.refuse(&start);
    let failure = worker.join().unwrap().err().expect("real framed Ready Err");
    assert!(!failure.no_start_write);
    connection.eof();
    failure
}

/// Actual connected-socket guard and real writer checkpoint, with a synthetic
/// backend Ready. It qualifies client/storage sequencing, not Host/provider authority.
pub(in crate::app) fn checkpointed_start_ready_fixture(
    app: &mut Cagents,
    target: crate::app::chat_ui::Target,
    session_id: Option<&str>,
) -> (Arc<crate::chat::client::ClientHandle>, JoinHandle<usize>) {
    let (handle, worker, _) = checkpointed_start_ready_update_fixture(app, target, session_id);
    (handle, worker)
}

fn checkpointed_start_ready_update_fixture(
    app: &mut Cagents,
    target: crate::app::chat_ui::Target,
    session_id: Option<&str>,
) -> (
    Arc<crate::chat::client::ClientHandle>,
    JoinHandle<usize>,
    Arc<std::sync::Mutex<UnixStream>>,
) {
    let entry = app
        .unresolved_start_custody()
        .find(|entry| entry.card == target.card)
        .unwrap()
        .clone();
    let (mut connection, start, worker) =
        start_through_checkpoint(app, target, entry.source.clone(), entry.operation);
    let mut owner = entry.source;
    owner.proof = Some(proof());
    owner.launch.session_id = session_id.map(str::to_owned);
    connection.response(
        &start,
        ServerResponse::ChatAttached {
            session: owner.clone(),
            snapshot: crate::chat::Snapshot {
                ready: true,
                session_id: owner.launch.session_id.clone(),
                ..Default::default()
            },
        },
    );
    let handle = Arc::new(worker.join().unwrap().unwrap());
    connection
        .0
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let updates = Arc::new(std::sync::Mutex::new(connection.0.try_clone().unwrap()));
    let writes = updates.clone();
    let peer = thread::spawn(move || {
        let mut prompts = 0;
        while let Some(request) = read_frame::<ClientEnvelope>(&mut connection.0).unwrap() {
            assert_eq!(request.proof.as_ref(), Some(&proof()));
            let response = match &request.request {
                ClientRequest::ChatVisibility { .. } => ServerResponse::Accepted,
                ClientRequest::ChatRead { chat_id, .. } => ServerResponse::ChatSnapshot {
                    chat_id: *chat_id,
                    snapshot: None,
                },
                ClientRequest::ChatCommand { command, .. } => {
                    if matches!(command, crate::chat::Command::Prompt { .. }) {
                        prompts += 1;
                    }
                    ServerResponse::Accepted
                }
                ClientRequest::ChatDetach { .. } => ServerResponse::Detached,
                ClientRequest::ChatClose { chat_id } => ServerResponse::ChatClosed {
                    chat_id: *chat_id,
                    snapshot: Default::default(),
                },
                other => panic!("unexpected synthetic backend request: {other:?}"),
            };
            write_frame(
                &mut *writes.lock().unwrap(),
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: proof(),
                    message: ServerMessage::Response { response },
                },
            )
            .unwrap();
        }
        prompts
    });
    (handle, peer, updates)
}

fn refused_custody_peer(
    original: Authority,
    registry: uuid::Uuid,
    owner: crate::chat::Session,
) -> Peer {
    Peer::new(move |server| {
        let mut resolve = server.connect(&original, Some(registry), false).unwrap();
        resolve.chats(vec![owner.clone()]);
        resolve.eof();
        let mut close = server.connect(&original, Some(registry), false).unwrap();
        close.chats(vec![owner.clone()]);
        let request = close.close_chat(&owner);
        close.refuse(&request);
        let detach = close.next();
        assert!(
            matches!(detach.request, ClientRequest::ChatDetach { chat_id } if chat_id == owner.id)
        );
        close.response(&detach, ServerResponse::Detached);
        close.eof();
        let mut retained = server.connect(&original, Some(registry), false).unwrap();
        let request = retained.next();
        assert!(
            matches!(&request.request, ClientRequest::ChatCloseRetained { session } if session == &owner)
        );
        retained.refuse(&request);
        retained.eof();
    })
}

fn accepted_custody_peer(registry: uuid::Uuid, owner: crate::chat::Session) -> Peer {
    accepted_custody_identity_peer(registry, owner, None)
}

fn accepted_custody_identity_peer(
    registry: uuid::Uuid,
    owner: crate::chat::Session,
    learned: Option<crate::chat::Session>,
) -> Peer {
    Peer::new(move |server| {
        let mut resolve = server
            .connect(&Authority::default(), Some(registry), false)
            .unwrap();
        resolve.chats(vec![owner.clone()]);
        resolve.eof();
        let mut close = server
            .connect(&Authority::default(), Some(registry), false)
            .unwrap();
        if let Some(learned) = learned {
            close.chats(vec![learned]);
            close.eof();
            let mut retained = server
                .connect(&Authority::default(), Some(registry), false)
                .unwrap();
            let request = retained.next();
            assert!(
                matches!(&request.request, ClientRequest::ChatCloseRetained { session } if session == &owner)
            );
            retained.response(&request, ServerResponse::Accepted);
            retained.eof();
            return;
        }
        close.chats(vec![owner.clone()]);
        let request = close.close_chat(&owner);
        close.response(
            &request,
            ServerResponse::ChatClosed {
                chat_id: owner.id,
                snapshot: Default::default(),
            },
        );
        close.eof();
    })
}

fn publish_custody_identity(
    updates: &std::sync::Mutex<UnixStream>,
    handle: &crate::chat::client::ClientHandle,
    learned: &crate::chat::Session,
) {
    write_frame(
        &mut *updates.lock().unwrap(),
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: None,
            proof: proof(),
            message: ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id: learned.id,
                    snapshot: Some(crate::chat::Snapshot {
                        ready: true,
                        generation: 1,
                        session_id: learned.launch.session_id.clone(),
                        ..Default::default()
                    }),
                },
            },
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while handle.session().launch.session_id != learned.launch.session_id {
        assert!(
            Instant::now() < deadline,
            "framed UUID publication did not arrive"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn user_custody_fixture_launch() {
    if std::env::var_os(ROLE).is_none() {
        for name in [
            crate::agent_execution::AUTHORITY_ENV,
            "CAGENTS_AGENT_TOKEN",
            "CAGENTS_SESSION_TOKEN",
        ] {
            assert!(
                std::env::var_os(name).is_none(),
                "fixture requires genuine User/CI before isolation"
            );
        }
    }
}

fn pending_close_waits_for_its_exact_outcome(_role: &str) {
    use crate::app::chat_ui;
    for window in ["before-ready", "ready-save-ack", "ready-save-failure"] {
        let mut app = custody_app(window);
        let registry = app.workspace_model.identity;
        let (card, target, raw, start_operation) =
            chat_ui::tests::attempted_claude_start_fixture(&mut app);
        let (handle, backend) = checkpointed_start_ready_fixture(
            &mut app,
            target,
            Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
        );
        let owner = handle.session();
        assert_eq!(owner.id, raw.id);
        if window == "before-ready" {
            drop(app.begin_chat_close(card));
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(app.chat_has_unresolved_start_custody(card));
            let peer = accepted_custody_peer(registry, owner);
            let task =
                chat_ui::tests::deliver_attempted_start_fixture(&mut app, target, Ok(handle));
            drain_custody(&mut app, task);
            peer.finished();
            assert!(!app.card_state(card).unwrap().existence.is_open());
            assert!(!app.chat_has_unresolved_start_custody(card));
            drop(app);
            assert_eq!(
                backend.join().unwrap(),
                0,
                "a Close before Ready must never dispatch the queued prompt"
            );
            continue;
        }
        if window == "ready-save-failure" {
            app.session
                .as_ref()
                .unwrap()
                .operation_store()
                .shutdown()
                .unwrap();
        }
        let task = chat_ui::tests::deliver_attempted_start_fixture(&mut app, target, Ok(handle));
        let ack = custody_message(task).expect("Ready descriptor and custody release save");
        assert!(
            matches!(&ack, Message::ChatUi(chat_ui::Action::StartCustodyReleased(entry, false, false, result)) if entry.operation == start_operation && result.is_err() == (window == "ready-save-failure"))
        );
        assert!(
            app.chat_has_unresolved_start_custody(card),
            "the save waiter owns custody until its exact ACK is applied"
        );
        if window == "ready-save-ack" {
            // The durable release ACK is already available, but has not run in
            // the UI. Close must survive this window and still close the owner.
            drop(app.begin_chat_close(card));
            assert!(app.card_state(card).unwrap().existence.is_open());
            let peer = accepted_custody_peer(registry, owner);
            let task = app.update(ack);
            drain_custody(&mut app, task);
            peer.finished();
        } else {
            drop(app.update(ack));
            let retry = app
                .unresolved_start_custody()
                .find(|entry| entry.card == card)
                .unwrap();
            assert_ne!(retry.operation, start_operation);
            assert_eq!(retry.source, raw);
            assert_eq!(retry.observed.as_ref(), Some(&owner));
            assert!(retry.failure.is_some());
            assert!(!app.chat_start_pending(card));
            chat_ui::tests::edit_attempted_start_fixture(
                &mut app,
                card,
                "Keep this after failed save λ  ",
            );
            let draft = chat_ui::tests::attempted_start_draft_fixture(&app, card);
            assert!(!draft.is_empty());
            assert!(app.card_state(card).unwrap().existence.is_open());
            let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
            let (writer, _) = crate::session::SessionRuntime::open(
                root.join(format!("{window}.sqlite3")),
                crate::model::BootId::new("recovered-writer"),
            )
            .unwrap();
            app.session = Some(writer);
            assert_eq!(
                chat_ui::tests::attempted_start_draft_fixture(&app, card),
                draft
            );
            let peer = accepted_custody_peer(registry, owner);
            let task = app.begin_chat_close(card);
            drain_custody(&mut app, task);
            peer.finished();
        }
        assert!(!app.chat_has_unresolved_start_custody(card));
        assert!(!app.card_state(card).unwrap().existence.is_open());
        let manifest = app
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .load_workspace_manifest(app.durable.id)
            .unwrap()
            .unwrap();
        assert!(manifest.native_start_custody.is_empty());
        assert!(manifest.released_native_starts.contains(&start_operation));
        drop(app);
        assert!(
            backend.join().unwrap() <= 1,
            "cleanup cannot replay a prompt"
        );
    }
}

#[test]
fn close_survives_pending_ready_and_save_ack_and_failed_save_keeps_retryable_custody() {
    user_custody_fixture_launch();
    isolated(&["user"], pending_close_waits_for_its_exact_outcome);
}

fn mounted_identity_learning_has_a_finite_pane_owner(_role: &str) {
    use crate::app::chat_ui;
    for close_pane in [false, true] {
        for change in [
            "learned",
            "pre-close-unprojected",
            "post-close-unprojected",
            "descriptor-only",
            "connection",
            "same-descriptor-connection",
            "target",
        ] {
            let mut app = custody_app(&format!("{change}-{close_pane}"));
            let registry = app.workspace_model.identity;
            let (card, target, _, _) = chat_ui::tests::attempted_claude_start_fixture(&mut app);
            let (handle, backend, updates) =
                checkpointed_start_ready_update_fixture(&mut app, target, None);
            let original = handle.session();
            assert!(original.launch.session_id.is_none());
            let mut learned = original.clone();
            learned.launch.session_id = Some("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into());
            let same_owner = matches!(
                change,
                "learned" | "pre-close-unprojected" | "post-close-unprojected"
            );
            let task = chat_ui::tests::deliver_attempted_start_fixture(
                &mut app,
                target,
                Ok(handle.clone()),
            );
            let ready_ack = custody_message(task).unwrap();
            chat_ui::tests::edit_attempted_start_fixture(
                &mut app,
                card,
                "Retain this edited λ draft  ",
            );
            let draft = chat_ui::tests::attempted_start_draft_fixture(&app, card);
            if close_pane {
                drop(app.begin_chat_close(card));
            } else {
                drop(app.retry_start_custody(card, false));
            }
            let peer = accepted_custody_identity_peer(
                registry,
                original.clone(),
                (change == "pre-close-unprojected").then(|| learned.clone()),
            );
            let task = app.update(ready_ack);
            let resolved = custody_message(task).unwrap();
            let task = app.update(resolved);
            let observed_ack = custody_message(task).unwrap();
            if change == "pre-close-unprojected" {
                publish_custody_identity(&updates, &handle, &learned);
                assert!(
                    matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(current) if current == &original)
                );
            }
            let task = app.update(observed_ack); // captures the actual pane owner before Close
            let closed = custody_message(task).unwrap();
            assert!(matches!(
                &closed,
                Message::ChatUi(chat_ui::Action::StartCleanupFinished(_, requested, Ok(()))) if *requested == close_pane
            ));
            peer.finished();
            if change == "same-descriptor-connection" {
                learned = original.clone();
                chat_ui::tests::replace_custody_pane_lifetime_fixture(&mut app, card, true);
            } else if change == "descriptor-only" {
                // An accepted descriptor install alone must not erase a different
                // mounted source, even when only the provider ID differs.
                app.install_chat_session(card, learned.clone());
            } else {
                // Real framed metadata publication on the exact original client,
                // followed by the ordinary Wake projection into durable state.
                if change != "pre-close-unprojected" {
                    publish_custody_identity(&updates, &handle, &learned);
                }
                if !change.ends_with("unprojected") {
                    drop(app.update(Message::ChatUi(chat_ui::Action::Wake(
                        target,
                        crate::chat::client::Wake::Changed,
                    ))));
                    assert!(
                        matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(current) if current == &learned)
                    );
                } else {
                    assert!(
                        matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(current) if current == &original),
                        "reader publication must not be mistaken for UI projection"
                    );
                }
                if change == "connection" || change == "target" {
                    chat_ui::tests::replace_custody_pane_lifetime_fixture(
                        &mut app,
                        card,
                        change == "connection",
                    );
                }
            }
            let task = app.update(closed);
            drain_custody(&mut app, task);
            assert!(!app.chat_has_unresolved_start_custody(card));
            assert_eq!(
                app.card_state(card).unwrap().existence.is_open(),
                !close_pane || !same_owner,
                "only the captured actor's mounted pane can be retired after UUID learning"
            );
            if !close_pane && same_owner {
                assert!(
                    matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(current) if current.id != original.id && current.is_unstarted())
                );
                assert_eq!(
                    chat_ui::tests::attempted_start_draft_fixture(&app, card),
                    draft
                );
                assert!(
                    !app.chat_start_pending(card),
                    "explicit cleanup opens an inert fresh draft, never a Start replay"
                );
            }
            if !same_owner {
                assert!(
                    app.notices.entries.iter().any(|entry| entry.notice.message == "The previous conversation stopped. This pane changed while cleanup was pending, so its current conversation was kept."),
                    "a preserved replacement needs an explanation"
                );
                assert!(
                    matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(current) if current == &learned)
                );
            }
            drop(app);
            drop(handle);
            drop(updates);
            assert!(backend.join().unwrap() <= 1);
        }
    }
}

#[test]
fn mounted_learned_uuid_closes_only_its_captured_connection_and_preserves_replacements() {
    user_custody_fixture_launch();
    isolated(&["user"], mounted_identity_learning_has_a_finite_pane_owner);
}

fn restored_user_custody_refuses_legacy_role_inputs(_role: &str) {
    use crate::app::chat_ui;
    for name in ["CAGENTS_AGENT_TOKEN", "CAGENTS_SESSION_TOKEN"] {
        let mut app = custody_app(name);
        let (card, target, raw, operation) =
            chat_ui::tests::attempted_claude_start_fixture(&mut app);
        let failure = actual_start_error(&mut app, target, raw, operation);
        // No provider/auth/close connection is permitted with either legacy
        // role input present, including an empty value.
        unsafe {
            std::env::set_var(name, "");
        }
        let peer = Peer::new(|_| {});
        let task = chat_ui::tests::deliver_attempted_start_fixture(&mut app, target, Err(failure));
        drain_custody(&mut app, task);
        app.persist();
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(10))
            .unwrap();
        let manifest = app
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .load_workspace_manifest(app.durable.id)
            .unwrap()
            .unwrap();
        assert_eq!(manifest.native_start_custody.len(), 1);
        assert!(manifest.native_start_custody[0].caller.is_none());
        Arc::make_mut(&mut app.durable).native_start_custody = manifest.native_start_custody;
        app.chat_ui = crate::app::chat_ui::State::default();
        let task = app.begin_chat_close(card);
        assert!(custody_message(task).is_none());
        assert!(app.chat_has_unresolved_start_custody(card));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(!app.chat_start_pending(card));
        peer.finished();
        // Inert proven-no-write metadata needs no provider authority. Retain
        // the legacy input and show that a separate fresh draft can be closed.
        let (fresh, fresh_target, raw, operation) =
            chat_ui::tests::attempted_claude_start_fixture(&mut app);
        let (stream, mut rejected) = UnixStream::pair().unwrap();
        rejected
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let no_write = crate::chat::client::ClientHandle::fixture_guarded_start(
            stream,
            raw,
            operation,
            |_| panic!("a scoped fresh start must refuse before handshake"),
        )
        .err()
        .unwrap();
        assert!(no_write.no_start_write);
        assert!(
            read_frame::<ClientEnvelope>(&mut rejected)
                .unwrap()
                .is_none()
        );
        let task =
            chat_ui::tests::deliver_attempted_start_fixture(&mut app, fresh_target, Err(no_write));
        drain_custody(&mut app, task);
        assert!(!app.chat_has_unresolved_start_custody(fresh));
        let task = app.begin_chat_close(fresh);
        drain_custody(&mut app, task);
        assert!(!app.card_state(fresh).unwrap().existence.is_open());
        unsafe {
            std::env::remove_var(name);
        }
    }
}

#[test]
fn restored_user_custody_with_legacy_role_presence_sends_no_resolve_or_close_frames() {
    user_custody_fixture_launch();
    isolated(&["user"], restored_user_custody_refuses_legacy_role_inputs);
}

fn attempted_start_failure_keeps_custody(role: &str) {
    use crate::app::chat_ui::{self, Action as ChatAction};
    let mut app = custody_app("failed-ready");
    let registry = app.workspace_model.identity;
    let (card, target, raw, operation) = chat_ui::tests::attempted_claude_start_fixture(&mut app);
    assert!(raw.is_unstarted());
    let failure = actual_start_error(&mut app, target, raw.clone(), operation);
    let owner = synthetic_owner(&raw);
    let peer = refused_custody_peer(authority(role), registry, owner.clone());
    let task = chat_ui::tests::deliver_attempted_start_fixture(&mut app, target, Err(failure));
    drain_custody(&mut app, task);
    peer.finished();
    assert!(
        !app.chat_start_pending(card),
        "failed custody is settled, not a perpetual spinner"
    );
    assert!(app.chat_has_unresolved_start_custody(card));
    assert_eq!(
        chat_ui::tests::attempted_start_draft_fixture(&app, card),
        "hello"
    );
    let saved = app.card_state(card).unwrap().clone();
    let focus = app.workspace.focused();
    let entry = app
        .unresolved_start_custody()
        .find(|entry| entry.operation == operation)
        .unwrap();
    assert_eq!(entry.source, raw);
    assert_eq!(entry.observed.as_ref(), Some(&owner));
    assert!(entry.failure.is_some());
    chat_ui::tests::edit_attempted_start_fixture(&mut app, card, "\nKeep this edited λ draft  ");
    let draft = chat_ui::tests::attempted_start_draft_fixture(&app, card);
    drop(app.update(Message::ChatUi(ChatAction::Submit(
        target,
        crate::chat::PromptMode::Send,
    ))));
    assert_eq!(app.card_state(card), Some(&saved));
    assert!(!app.chat_start_pending(card));
    assert_eq!(
        chat_ui::tests::attempted_start_draft_fixture(&app, card),
        draft
    );
    // Offline retry cannot spend an empty inventory or clear custody.
    let task = app.begin_chat_close(card);
    drain_custody(&mut app, task);
    assert!(app.chat_has_unresolved_start_custody(card));
    assert_eq!(app.workspace.focused(), focus);
    assert_eq!(
        chat_ui::tests::attempted_start_draft_fixture(&app, card),
        draft
    );
    // The same actual observed alias can learn its UUID while a Close is
    // delayed. Keep the immutable old descriptor and ask existing authenticated
    // retained cleanup to validate that alias; never close the newer census
    // descriptor merely because its allocation ID matches.
    let old_alias = owner.clone();
    let mut learned = owner.clone();
    learned.launch.session_id = Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into());
    let original = authority(role);
    let peer = Peer::new(move |server| {
        let mut resolve = server.connect(&original, Some(registry), false).unwrap();
        resolve.chats(vec![learned.clone()]);
        resolve.eof();
        let mut close = server.connect(&original, Some(registry), false).unwrap();
        close.chats(vec![learned]);
        close.eof();
        let mut retained = server.connect(&original, Some(registry), false).unwrap();
        let request = retained.next();
        assert!(
            matches!(&request.request, ClientRequest::ChatCloseRetained { session } if session == &old_alias && session.launch.session_id.is_none())
        );
        retained.response(&request, ServerResponse::Accepted);
        retained.eof();
    });
    let task = app.begin_chat_close(card);
    drain_custody(&mut app, task);
    peer.finished();
    assert!(!app.chat_has_unresolved_start_custody(card));
    assert!(!app.card_state(card).unwrap().existence.is_open());
    let manifest = app
        .session
        .as_ref()
        .unwrap()
        .operation_store()
        .load_workspace_manifest(app.durable.id)
        .unwrap()
        .unwrap();
    assert!(manifest.native_start_custody.is_empty());
    assert!(manifest.released_native_starts.contains(&operation));
}

#[test]
fn first_start_ready_error_and_failed_retirement_keep_exact_custody_until_positive_close() {
    if std::env::var_os(ROLE).is_none() {
        for name in [
            crate::agent_execution::AUTHORITY_ENV,
            "CAGENTS_AGENT_TOKEN",
            "CAGENTS_SESSION_TOKEN",
        ] {
            assert!(
                std::env::var_os(name).is_none(),
                "fixture requires a genuine User/CI launch; inherited role presence must not be stripped"
            );
        }
    }
    isolated(&["user"], attempted_start_failure_keeps_custody);
}

fn scoped_first_start_is_proven_no_effect(role: &str) {
    use crate::app::chat_ui;
    let mut app = custody_app(&format!("scoped-no-effect-{role}"));
    let (card, target, raw, operation) = chat_ui::tests::attempted_claude_start_fixture(&mut app);
    let (stream, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let failure =
        crate::chat::client::ClientHandle::fixture_guarded_start(stream, raw, operation, |_| {
            panic!("no scoped start checkpoint/connection is permitted")
        })
        .err()
        .unwrap();
    assert!(failure.no_start_write);
    assert!(
        read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
        "no Connect, Authenticate, Start, or User fallback"
    );
    let task = chat_ui::tests::deliver_attempted_start_fixture(&mut app, target, Err(failure));
    drain_custody(&mut app, task);
    assert!(!app.chat_has_unresolved_start_custody(card));
    assert_eq!(
        chat_ui::tests::attempted_start_draft_fixture(&app, card),
        "hello"
    );
    drop(app.begin_chat_close(card));
    assert!(
        !app.card_state(card).unwrap().existence.is_open(),
        "a no-effect draft remains removable offline"
    );
}

#[test]
fn scoped_fresh_start_refuses_before_connection_without_stranding_the_draft() {
    if std::env::var_os(ROLE).is_none() {
        for name in [
            crate::agent_execution::AUTHORITY_ENV,
            "CAGENTS_AGENT_TOKEN",
            "CAGENTS_SESSION_TOKEN",
        ] {
            assert!(
                std::env::var_os(name).is_none(),
                "fixture requires a genuine User/CI launch before isolation"
            );
        }
    }
    isolated(
        &["session", "empty", "non-unicode"],
        scoped_first_start_is_proven_no_effect,
    );
}

fn cancelled_first_start_never_writes(_role: &str) {
    use crate::app::chat_ui::{self, Action as ChatAction};
    for cancellation in [
        "cancel",
        "close",
        "source",
        "source-aba",
        "writer",
        "workspace",
    ] {
        let mut app = custody_app(&format!("before-ack-{cancellation}"));
        let (card, target, raw, operation) =
            chat_ui::tests::attempted_claude_start_fixture(&mut app);
        let original = raw.clone();
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (events, receive) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            crate::chat::client::ClientHandle::fixture_guarded_start(
                stream,
                raw,
                operation,
                move |wake| {
                    events.send(wake).unwrap();
                },
            )
        });
        let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        let mut connection = Connection(peer);
        connection.response(
            &hello,
            ServerResponse::Connected {
                proof: proof(),
                resumed: vec![],
            },
        );
        let wake = receive.recv_timeout(Duration::from_secs(5)).unwrap();
        let checkpoint = app.update(Message::ChatUi(ChatAction::Wake(target, wake)));
        let ack = custody_message(checkpoint).unwrap();
        let mut replacement = original.clone();
        replacement.id = crate::model::TerminalId::new();
        let original_workspace = app.durable.id;
        match cancellation {
            "cancel" => drop(app.update(Message::ChatUi(ChatAction::CancelQueued(target)))),
            "close" => drop(app.begin_chat_close(card)),
            "source" => {
                app.install_chat_session(card, replacement.clone());
            }
            "source-aba" => {
                app.install_chat_session(card, replacement.clone());
                app.install_chat_session(card, original.clone());
            }
            "writer" => {
                let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
                let (writer, _) = crate::session::SessionRuntime::open(
                    root.join("replacement-writer.sqlite3"),
                    crate::model::BootId::new("replacement-writer"),
                )
                .unwrap();
                app.session = Some(writer);
            }
            "workspace" => Arc::make_mut(&mut app.durable).id = crate::model::WorkspaceId::new(),
            _ => unreachable!(),
        }
        assert!(app.chat_has_unresolved_start_custody(card));
        drop(app.update(ack));
        let failure = worker.join().unwrap().err().unwrap();
        assert!(
            failure.no_start_write,
            "cancellation invalidates exact admission before its ACK"
        );
        connection.eof();
        // Re-enter the original local workspace before settling its certified
        // no-write outcome; no callback is allowed to clear a different window.
        Arc::make_mut(&mut app.durable).id = original_workspace;
        let task = chat_ui::tests::deliver_attempted_start_fixture(&mut app, target, Err(failure));
        drain_custody(&mut app, task);
        assert!(!app.chat_has_unresolved_start_custody(card));
        if cancellation == "close" {
            assert!(!app.card_state(card).unwrap().existence.is_open());
            continue;
        }
        assert!(app.card_state(card).unwrap().existence.is_open());
        if cancellation == "source" {
            assert!(
                matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(current) if current == &replacement)
            );
        }
        assert_eq!(
            chat_ui::tests::attempted_start_draft_fixture(&app, card),
            "hello"
        );
    }
}

#[test]
fn cancellation_close_source_aba_writer_and_window_changes_before_ack_send_no_start() {
    if std::env::var_os(ROLE).is_none() {
        for name in [
            crate::agent_execution::AUTHORITY_ENV,
            "CAGENTS_AGENT_TOKEN",
            "CAGENTS_SESSION_TOKEN",
        ] {
            assert!(
                std::env::var_os(name).is_none(),
                "fixture requires genuine User/CI launch before isolation"
            );
        }
    }
    isolated(&["user"], cancelled_first_start_never_writes);
}

fn empty_inventory_keeps_replaced_and_tombstoned_sources(_role: &str) {
    use crate::app::chat_ui;
    let (mut app, channel, _, _) = tests::channel_with_a_conversation();
    let registry = uuid::Uuid::new_v4();
    app.workspace_model.identity = registry;
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(registry);
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let (writer, _) = crate::session::SessionRuntime::open(
        root.join("replaced.sqlite3"),
        crate::model::BootId::new("custody-test"),
    )
    .unwrap();
    app.session = Some(writer);
    let (card, target, raw, operation) = chat_ui::tests::attempted_claude_start_fixture(&mut app);
    let project = app
        .durable
        .channels
        .iter()
        .find(|entry| entry.id == channel)
        .unwrap()
        .project_id;
    let failure = actual_start_error(&mut app, target, raw.clone(), operation);
    let peer = Peer::new(move |server| {
        let mut connection = server
            .connect(&Authority::default(), Some(registry), false)
            .unwrap();
        connection.chats(vec![]);
        connection.eof();
    });
    let task = chat_ui::tests::deliver_attempted_start_fixture(&mut app, target, Err(failure));
    drain_custody(&mut app, task);
    peer.finished();
    assert!(app.chat_has_unresolved_start_custody(card));
    assert!(
        app.unresolved_start_custody()
            .find(|entry| entry.operation == operation)
            .unwrap()
            .observed
            .is_none()
    );
    let (tx, publications) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    let before = app.card_state(card).unwrap().clone();
    let channels = app.durable.channels.clone();
    let focused = app.workspace.focused();
    let draft = chat_ui::tests::attempted_start_draft_fixture(&app, card);
    drop(app.delete_channel(channel));
    drop(app.retire_peer_channel(channel));
    assert_eq!(app.durable.channels, channels);
    assert_eq!(app.card_state(card), Some(&before));
    assert_eq!(app.workspace.focused(), focused);
    assert_eq!(
        chat_ui::tests::attempted_start_draft_fixture(&app, card),
        draft
    );
    assert!(
        publications.try_recv().is_err(),
        "no DeleteChannel/CloseProject may escape custody"
    );
    assert!(app.project_start_custody_blocker(project).is_some());
    // Authoritative replacement cannot erase the old attempted-source ledger.
    app.chat_forget(card);
    app.cards.remove(&card);
    let mut replacement = raw.clone();
    replacement.id = crate::model::TerminalId::new();
    app.replace_chat_conversation(card, replacement.clone());
    assert!(app.chat_has_unresolved_start_custody(card));
    let mut observed = synthetic_owner(&raw);
    observed.proof.as_mut().unwrap().host_incarnation = "replacement-epoch".into();
    let peer = Peer::new(move |server| {
        let mut connection = server
            .connect(&Authority::default(), Some(registry), false)
            .unwrap();
        connection.chats(vec![observed]);
        connection.eof();
    });
    let task = app.begin_chat_close(card);
    drain_custody(&mut app, task);
    peer.finished();
    assert!(
        matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(current) if current == &replacement)
    );
    assert!(app.chat_has_unresolved_start_custody(card));
    assert_eq!(
        app.persistent_workspace().chat_drafts.get(&card),
        Some(&draft)
    );
    // Late legacy retirement and another nonce cannot settle this custody.
    drop(app.update(Message::ChatUi(chat_ui::Action::Retired(target, Ok(())))));
    drop(
        app.update(Message::ChatUi(chat_ui::Action::StartCleanupFinished(
            uuid::Uuid::new_v4(),
            true,
            Ok(()),
        ))),
    );
    assert!(app.chat_has_unresolved_start_custody(card));
    chat_ui::tests::tombstone_attempted_start_fixture(&mut app, card);
    assert!(!app.card_state(card).unwrap().existence.is_open());
    app.persist();
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(10))
        .unwrap();
    assert!(app.channel_start_custody_blocker(channel).is_some());
    assert!(app.project_start_custody_blocker(project).is_some());
    let manifest = app
        .session
        .as_ref()
        .unwrap()
        .operation_store()
        .load_workspace_manifest(app.durable.id)
        .unwrap()
        .unwrap();
    assert_eq!(manifest.native_start_custody.len(), 1);
    assert_eq!(manifest.native_start_custody[0].source, raw);
    assert_eq!(manifest.native_start_custody[0].destination, Some(proof()));
    // Restore the real saved metadata with no live attempt/context; an explicit
    // retry still refuses an empty inventory without issuing Close or Start.
    Arc::make_mut(&mut app.durable).native_start_custody = manifest.native_start_custody;
    app.chat_ui = crate::app::chat_ui::State::default();
    let peer = Peer::new(move |server| {
        let mut connection = server
            .connect(&Authority::default(), Some(registry), false)
            .unwrap();
        connection.chats(vec![]);
        connection.eof();
    });
    let task = app.retry_start_custody_operation(operation, false);
    drain_custody(&mut app, task);
    peer.finished();
    assert!(app.chat_has_unresolved_start_custody(card));
}

#[test]
fn uncertain_start_absence_epoch_replacement_and_restart_preserve_scope_and_local_work() {
    if std::env::var_os(ROLE).is_none() {
        for name in [
            crate::agent_execution::AUTHORITY_ENV,
            "CAGENTS_AGENT_TOKEN",
            "CAGENTS_SESSION_TOKEN",
        ] {
            assert!(
                std::env::var_os(name).is_none(),
                "fixture requires genuine User/CI before isolation"
            );
        }
    }
    isolated(
        &["user"],
        empty_inventory_keeps_replaced_and_tombstoned_sources,
    );
}

fn protected_neighbor_custody_never_becomes_a_removal_close(_role: &str) {
    use crate::app::{chat_ui, project_ui::Action as ProjectAction};
    let (mut app, channel, _, _) = tests::channel_with_a_conversation();
    let owner = app
        .durable
        .channels
        .iter()
        .find(|entry| entry.id == channel)
        .unwrap()
        .project_id;
    let target = crate::model::ProjectId::new();
    let row = crate::sidebar::SidebarProject::new(
        target,
        crate::model::RepositoryId::new(),
        "Neighbor with same cwd".into(),
        "/repo".into(),
        vec![],
        vec![],
    );
    app.sidebar_model.projects.push(row);
    let registry = uuid::Uuid::new_v4();
    app.workspace_model.identity = registry;
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(registry);
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let (writer, _) = crate::session::SessionRuntime::open(
        root.join("protected-neighbor.sqlite3"),
        crate::model::BootId::new("custody-test"),
    )
    .unwrap();
    app.session = Some(writer);
    let (card, generation, raw, operation) =
        chat_ui::tests::attempted_claude_start_in_directory_fixture(&mut app, "/repo".into());
    let failure = actual_start_error(&mut app, generation, raw.clone(), operation);
    let peer = Peer::new(move |server| {
        let mut connection = server
            .connect(&Authority::default(), Some(registry), false)
            .unwrap();
        connection.chats(vec![]);
        connection.eof();
    });
    let task = chat_ui::tests::deliver_attempted_start_fixture(&mut app, generation, Err(failure));
    drain_custody(&mut app, task);
    peer.finished();
    let entry = app
        .unresolved_start_custody()
        .find(|entry| entry.operation == operation)
        .unwrap();
    assert_eq!(entry.project, Some(owner));
    assert_eq!(entry.channel, Some(channel));
    assert_eq!(
        entry.source.launch.recipe.working_directory,
        PathBuf::from("/repo")
    );
    assert!(!app.project_owns_start_custody(target, entry));
    assert!(app.project_owns_start_custody(owner, entry));
    assert!(
        app.project_start_custody_blocker(target).is_none(),
        "neighbor custody must not be collected by cwd"
    );
    let (tx, publications) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    drop(app.update(Message::ProjectUi(ProjectAction::Command {
        project_id: target,
        worktree_id: None,
        command: crate::sidebar::ProjectCommand::Remove,
    })));
    // Even a queued cleanup action naming B's exact nonce cannot turn A's
    // confirmation into permission to close B. No async cleanup task exists.
    let task = app.update(Message::ProjectUi(ProjectAction::CleanupStart(
        target, operation,
    )));
    assert!(custody_message(task).is_none());
    assert!(!app.start_custody_cleanup_pending(operation));
    assert!(app.chat_has_unresolved_start_custody(card));
    assert!(publications.try_recv().is_err());
    // Ordinary Remove on the owning project only refuses. The separate
    // conversation action is the sole place that may request cleanup.
    drop(app.update(Message::ProjectUi(ProjectAction::Dismiss)));
    drop(app.update(Message::ProjectUi(ProjectAction::Command {
        project_id: owner,
        worktree_id: None,
        command: crate::sidebar::ProjectCommand::Remove,
    })));
    let task = app.update(Message::ProjectUi(ProjectAction::ConfirmRemove));
    assert!(custody_message(task).is_none());
    assert!(!app.start_custody_cleanup_pending(operation));
    assert!(app.chat_has_unresolved_start_custody(card));
    assert!(publications.try_recv().is_err());
    // The separate explicit cleanup is admitted while B owns the source. A
    // filing change after authenticated observation but before its durable ACK
    // must still prevent Close; the descriptor alone conveys no scope intent.
    let observed = synthetic_owner(&raw);
    let peer = Peer::new(move |server| {
        let mut connection = server
            .connect(&Authority::default(), Some(registry), false)
            .unwrap();
        connection.chats(vec![observed]);
        connection.eof();
    });
    let task = app.update(Message::ProjectUi(ProjectAction::CleanupStart(
        owner, operation,
    )));
    let resolved = custody_message(task).unwrap();
    let task = app.update(resolved);
    let ack = custody_message(task).unwrap();
    assert!(matches!(
        &ack,
        Message::ChatUi(chat_ui::Action::StartOwnerCheckpointed(_, _, false, Ok(())))
    ));
    Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|row| row.id == channel)
        .unwrap()
        .project_id = target;
    let task = app.update(ack);
    drain_custody(&mut app, task);
    peer.finished();
    assert!(!app.start_custody_cleanup_pending(operation));
    assert!(app.chat_has_unresolved_start_custody(card));
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert_eq!(
        chat_ui::tests::attempted_start_draft_fixture(&app, card),
        "hello"
    );
    assert!(publications.try_recv().is_err());
}

#[test]
fn protected_same_cwd_custody_is_never_closed_by_neighbor_or_ordinary_project_remove() {
    if std::env::var_os(ROLE).is_none() {
        for name in [
            crate::agent_execution::AUTHORITY_ENV,
            "CAGENTS_AGENT_TOKEN",
            "CAGENTS_SESSION_TOKEN",
        ] {
            assert!(
                std::env::var_os(name).is_none(),
                "genuine User/CI required before fixture isolation"
            );
        }
    }
    isolated(
        &["user"],
        protected_neighbor_custody_never_becomes_a_removal_close,
    );
}
