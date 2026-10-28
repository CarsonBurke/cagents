//! Private framed peers exercise the ordinary App close completion. They do
//! not authenticate a production owner or demonstrate provider retirement.
use super::*;
use crate::{
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
    repository::test_fixture::isolated_fixture_case,
    terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
};
use std::{
    io,
    os::unix::net::{UnixListener, UnixStream},
    thread,
};

fn fixture_proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("synthetic-retained-close-fixture"),
        host_incarnation: HostIncarnation::new("synthetic-retained-close-incarnation"),
        boot_id: BootId::new("synthetic-retained-close-boot"),
    }
}

fn retained_claude() -> crate::chat::Session {
    let mut session = chat_session();
    session.launch.provider = crate::agent::Provider::Claude;
    session.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
    session.proof = Some(fixture_proof());
    session.native_continuity = crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
    assert!(session.has_valid_native_continuity());
    session
}

fn app_with_retained_claude() -> (Cagents, ChannelId, CardId) {
    let (mut app, channel, survivor, card) = channel_with_a_conversation();
    // Keep the preexisting navigation scope independent of the channel being
    // removed, so an intentional leave does not obscure refusal custody.
    Arc::make_mut(&mut app.durable).active_channel = Some(survivor);
    app.card_state_mut(card).unwrap().kind = CardKind::Chat(retained_claude());
    let drafts = HashMap::from([(card, "Keep this unsent Claude draft 🦀".into())]);
    app.chat_ui = crate::app::chat_ui::State::with_drafts(drafts.clone());
    Arc::make_mut(&mut app.durable).chat_drafts = drafts;
    (app, channel, card)
}

fn private_socket() -> PathBuf {
    let runtime = PathBuf::from(std::env::var_os("CAGENTS_PR_FIXTURE_RUNTIME").unwrap());
    let socket = crate::terminal::ipc::daemon_socket_path().unwrap();
    assert!(
        socket.starts_with(&runtime),
        "only the isolated fixture endpoint is permitted"
    );
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    socket
}

fn assert_refused_close_keeps_local_work(mut app: Cagents, channel: ChannelId, card: CardId) {
    let saved = app.card_state(card).unwrap().clone();
    let channels = app.durable.channels.clone();
    let drafts = app.persistent_workspace().chat_drafts;
    let focused = app.workspace.focused();
    let active = app.durable.active_channel;
    let (tx, publications) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    let task = app.delete_channel(channel);
    assert!(app.channel_ui.is_deleting(channel));
    assert_eq!(app.card_state(card), Some(&saved));
    let completion = super::super::super::tests::task_app_message(task);
    assert!(matches!(&completion,
        Message::ChannelUi(Action::SessionsClosed(id, _, Err(_))) if *id == channel));
    drop(app.update(completion));
    assert!(!app.channel_ui.is_deleting(channel));
    assert!(app.channel_ui.failed.contains_key(&channel));
    assert!(!app.channel_ui.was_deleted(channel));
    assert_eq!(app.card_state(card), Some(&saved));
    assert_eq!(app.durable.channels, channels);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
    assert_eq!(app.workspace.focused(), focused);
    assert_eq!(app.durable.active_channel, active);
    assert!(
        std::iter::from_fn(|| publications.try_recv().ok())
            .all(|edit| { !matches!(edit.edit, ModelEdit::DeleteChannel { id } if id == channel) })
    );
}

#[test]
fn missing_or_stale_daemon_cannot_retire_retained_claude_local_work() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(missing_or_stale_daemon_cannot_retire_retained_claude_local_work)
    )) {
        return;
    }
    let socket = private_socket();
    for stale in [false, true] {
        if stale {
            drop(UnixListener::bind(&socket).unwrap());
        }
        let (app, channel, card) = app_with_retained_claude();
        assert_refused_close_keeps_local_work(app, channel, card);
        assert_eq!(
            socket.exists(),
            stale,
            "no daemon is started or stale endpoint removed"
        );
        if stale {
            std::fs::remove_file(&socket).unwrap();
        }
    }
    // Reconnect-only absence keeps its existing best-effort close policy.
    let ordinary = chat_session();
    assert!(close_sessions(vec![CardKind::Chat(ordinary)]).is_ok());
}

struct Connection(UnixStream);

impl Connection {
    fn response(&mut self, request: &ClientEnvelope, response: ServerResponse) {
        self.answer(request, ServerMessage::Response { response });
    }

    fn answer(&mut self, request: &ClientEnvelope, message: ServerMessage) {
        write_frame(
            &mut self.0,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(request.request_id),
                proof: fixture_proof(),
                message,
            },
        )
        .unwrap();
    }

    fn next(&mut self) -> ClientEnvelope {
        let request: ClientEnvelope = read_frame(&mut self.0).unwrap().unwrap();
        assert_eq!(request.proof.as_ref(), Some(&fixture_proof()));
        request
    }

    fn chats(&mut self, sessions: Vec<crate::chat::Session>) {
        let request = self.next();
        assert!(matches!(request.request, ClientRequest::ChatList));
        self.response(&request, ServerResponse::ChatListed { sessions });
    }

    fn refuse(&mut self, request: &ClientEnvelope) {
        self.answer(
            request,
            ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                "Synthetic retained close refusal",
                false,
            )),
        );
    }
}

fn accept(listener: &UnixListener) -> Connection {
    let deadline = Instant::now() + Duration::from_secs(5);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "private close connection was not attempted"
                );
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("private close accept: {error}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut connection = Connection(stream);
    let hello: ClientEnvelope = read_frame(&mut connection.0).unwrap().unwrap();
    assert!(matches!(hello.request, ClientRequest::Connect { .. }));
    assert!(hello.proof.is_none());
    connection.response(
        &hello,
        ServerResponse::Connected {
            proof: fixture_proof(),
            resumed: vec![],
        },
    );
    connection
}

#[test]
fn empty_or_disappearing_inventory_requires_exact_retained_close_before_app_deletion() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(
            empty_or_disappearing_inventory_requires_exact_retained_close_before_app_deletion
        )
    )) {
        return;
    }
    let socket = private_socket();
    for disappeared in [false, true] {
        for accepted in [false, true] {
            let (mut app, channel, card) = app_with_retained_claude();
            let CardKind::Chat(source) = app.card_state(card).unwrap().kind.clone() else {
                unreachable!()
            };
            let saved = app.card_state(card).unwrap().clone();
            let channels = app.durable.channels.clone();
            let drafts = app.persistent_workspace().chat_drafts;
            let focused = app.workspace.focused();
            let (tx, publications) = async_channel::unbounded();
            app.workspace_model_tx = tx;
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let peer = thread::spawn(move || {
                let mut connection = accept(&listener);
                let terminals = connection.next();
                assert!(matches!(terminals.request, ClientRequest::List));
                connection.response(&terminals, ServerResponse::Listed { terminals: vec![] });
                connection.chats(if disappeared {
                    vec![source.clone()]
                } else {
                    vec![]
                });
                if disappeared {
                    let attach = connection.next();
                    assert!(
                        matches!(&attach.request, ClientRequest::ChatAttach { session } if session == &source)
                    );
                    connection.response(
                        &attach,
                        ServerResponse::ChatAttached {
                            session: source.clone(),
                            snapshot: Default::default(),
                        },
                    );
                    let close = connection.next();
                    assert!(
                        matches!(close.request, ClientRequest::ChatClose { chat_id } if chat_id == source.id)
                    );
                    connection.refuse(&close);
                    drop(connection);
                    connection = accept(&listener);
                    connection.chats(vec![]);
                }
                // BEFORE may end after its valid empty census. Preserve that
                // EOF so the causal control fails the explicit receipt oracle.
                let Some(request) = read_frame::<ClientEnvelope>(&mut connection.0).unwrap() else {
                    return false;
                };
                assert_eq!(request.proof.as_ref(), Some(&fixture_proof()));
                assert!(
                    matches!(&request.request, ClientRequest::ChatCloseRetained { session } if session == &source)
                );
                if accepted {
                    connection.response(&request, ServerResponse::Accepted);
                } else {
                    connection.refuse(&request);
                }
                assert!(
                    read_frame::<ClientEnvelope>(&mut connection.0)
                        .unwrap()
                        .is_none()
                );
                true
            });
            let task = app.delete_channel(channel);
            assert!(app.channel_ui.is_deleting(channel));
            assert_eq!(app.card_state(card), Some(&saved));
            let completion = super::super::super::tests::task_app_message(task);
            let observed_receipt = peer.join().unwrap();
            std::fs::remove_file(&socket).unwrap();
            assert!(
                observed_receipt,
                "empty actor inventory must not replace the exact retained close receipt"
            );
            assert!(
                matches!(&completion, Message::ChannelUi(Action::SessionsClosed(id, _, result))
                if *id == channel && result.is_ok() == accepted)
            );
            drop(app.update(completion));
            if accepted {
                assert!(app.channel_ui.was_deleted(channel));
                assert!(!app.durable.channels.iter().any(|row| row.id == channel));
                assert!(!app.card_state(card).unwrap().existence.is_open());
                assert!(
                    std::iter::from_fn(|| publications.try_recv().ok()).any(|edit| {
                        matches!(edit.edit, ModelEdit::DeleteChannel { id } if id == channel)
                    })
                );
            } else {
                assert!(!app.channel_ui.was_deleted(channel));
                assert!(app.channel_ui.failed.contains_key(&channel));
                assert_eq!(app.durable.channels, channels);
                assert_eq!(app.card_state(card), Some(&saved));
                assert_eq!(app.persistent_workspace().chat_drafts, drafts);
                assert_eq!(app.workspace.focused(), focused);
                assert!(
                    !std::iter::from_fn(|| publications.try_recv().ok()).any(|edit| {
                        matches!(edit.edit, ModelEdit::DeleteChannel { id } if id == channel)
                    })
                );
            }
        }
    }
}
