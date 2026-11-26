//! A watch against a scripted daemon on the other end of a socket pair.

use std::{os::unix::net::UnixStream, sync::mpsc, time::Duration};

use super::*;
use crate::{
    agent::Provider,
    mobile::{Lines, Outgoing},
    model::{BootId, HostIncarnation, HostSessionId},
    terminal::ipc::ClientEnvelope,
};

const WAIT: Duration = Duration::from_secs(10);

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("session"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    }
}

fn session() -> chat::Session {
    chat::Session {
        id: TerminalId::new(),
        proof: None,
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
        launch: chat::Launch {
            provider: Provider::Claude,
            session_id: None,
            recipe: crate::model::LaunchRecipe {
                program: "claude".into(),
                arguments: Vec::new(),
                working_directory: "/fixture/project".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            draft_model: None,
        },
    }
}

/// The daemon's end of the connection.
struct Daemon(UnixStream);

impl Daemon {
    fn receive(&mut self) -> ClientEnvelope {
        read_frame::<ClientEnvelope>(&mut self.0)
            .unwrap()
            .expect("a request")
    }

    fn reply(&mut self, request: &ClientEnvelope, message: ServerMessage) {
        write_frame(
            &mut self.0,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(request.request_id),
                proof: proof(),
                message,
            },
        )
        .unwrap();
    }

    fn respond(&mut self, request: &ClientEnvelope, response: ServerResponse) {
        self.reply(request, ServerMessage::Response { response });
    }

    fn approve_watch(&mut self, chat_id: TerminalId) {
        let watch = self.receive();
        assert_eq!(
            watch.request,
            ClientRequest::WatchMobile {
                topic: MobileWatch::Conversation(chat_id),
            }
        );
        self.respond(&watch, ServerResponse::Accepted);
    }

    fn notify(&mut self, message: ServerMessage) {
        write_frame(
            &mut self.0,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: None,
                proof: proof(),
                message,
            },
        )
        .unwrap();
    }
}

fn start_scripted(
    id: u64,
    session: chat::Session,
    stream: UnixStream,
    proof: HostSessionProof,
    outbox: Outbox,
    notice: Option<uuid::Uuid>,
    ended: impl FnOnce(TerminalId, Option<String>) + Send + 'static,
) -> std::io::Result<Watch> {
    Watch::start(
        id,
        session.id,
        outbox,
        Arc::default(),
        move |stop| {
            stop.register(&stream)
                .map_err(|error| Failure::DaemonLost(error.to_string()))?;
            Ok(Opening {
                session,
                notice,
                stream,
                proof,
                next_request_id: 2,
            })
        },
        ended,
    )
}

fn next_line(lines: &Lines) -> api::ServerLine {
    match lines
        .outgoing
        .recv_timeout(WAIT)
        .expect("a line for the phone")
    {
        Outgoing::Line(line) => line,
        Outgoing::Exit => panic!("the bridge exited"),
        Outgoing::TerminalScreen(_) => panic!("unexpected terminal screen"),
    }
}

#[test]
fn a_completed_conversation_publishes_its_final_state_and_answers_pending_requests() {
    finish_watch(false);
}

#[test]
fn an_explicit_close_does_not_show_a_recovery_error() {
    finish_watch(true);
}

fn finish_watch(removed: bool) {
    let (bridge, daemon) = UnixStream::pair().unwrap();
    let (outbox, lines) = Outbox::new();
    let (ended, ending) = mpsc::channel();
    let session = session();
    let chat_id = session.id;
    let expected_ack = uuid::Uuid::new_v4();
    let watch = start_scripted(
        1,
        session.clone(),
        bridge,
        proof(),
        outbox,
        Some(expected_ack),
        move |chat, unfollowable| ended.send((chat, unfollowable)).unwrap(),
    )
    .unwrap();
    let mut daemon = Daemon(daemon);
    let attach = daemon.receive();
    assert!(matches!(attach.request, ClientRequest::ChatAttach { .. }));
    daemon.respond(
        &attach,
        ServerResponse::ChatAttached {
            session,
            snapshot: chat::Snapshot {
                generation: 1,
                ..chat::Snapshot::default()
            },
        },
    );
    let visibility = daemon.receive();
    assert!(matches!(
        visibility.request,
        ClientRequest::ChatVisibility { visible: true, .. }
    ));
    daemon.respond(&visibility, ServerResponse::Accepted);
    daemon.approve_watch(chat_id);
    assert_eq!(next_line(&lines), api::ServerLine::Ok { id: 1, page: None });
    let api::ServerLine::Event(api::Event::Conversation(update)) = next_line(&lines) else {
        panic!("the watch did not publish its first reset");
    };
    assert!(update.reset);
    let checkpoint = update.read_checkpoint.as_deref().unwrap().parse().unwrap();

    // Rewatching while hidden reads the next presentation and does not
    // acknowledge. A delayed read from another watch is refused.
    let read = daemon.receive();
    assert!(matches!(read.request, ClientRequest::ChatRead { .. }));
    assert!(watch.send(Command::Read {
        id: 2,
        checkpoint: uuid::Uuid::new_v4(),
    }));
    assert!(matches!(
        next_line(&lines),
        api::ServerLine::Error { id: Some(2), error } if error.code == api::ErrorCode::NotWatching
    ));
    assert!(watch.send(Command::Read { id: 3, checkpoint }));
    let acknowledge = daemon.receive();
    assert!(matches!(
        acknowledge.request,
        ClientRequest::ChatAcknowledge { chat_id: id, epoch: 1 } if id == chat_id
    ));
    assert!(lines.outgoing.try_recv().is_err());
    daemon.respond(&acknowledge, ServerResponse::Accepted);
    let dismiss = daemon.receive();
    assert!(matches!(
        dismiss.request,
        ClientRequest::AcknowledgeNotification { id } if id == expected_ack
    ));
    assert!(lines.outgoing.try_recv().is_err());
    daemon.respond(&dismiss, ServerResponse::Accepted);
    assert_eq!(next_line(&lines), api::ServerLine::Ok { id: 3, page: None });
    assert!(watch.send(Command::Chat {
        id: 7,
        command: chat::Command::Interrupt,
    }));
    let interrupt = daemon.receive();
    assert!(matches!(
        interrupt.request,
        ClientRequest::ChatCommand { .. }
    ));
    // A close from elsewhere arrives while a presentation is already in
    // flight. The bridge lets that read finish and asks once more.
    daemon.notify(ServerMessage::MobileChanged {
        topic: MobileWatch::Conversation(chat_id),
    });
    daemon.respond(
        &read,
        ServerResponse::ChatSnapshot {
            chat_id,
            snapshot: None,
        },
    );
    let final_read = daemon.receive();
    assert!(matches!(final_read.request, ClientRequest::ChatRead { .. }));
    let message = if removed {
        daemon.reply(
            &final_read,
            ServerMessage::Error(ProtocolError {
                code: ProtocolErrorCode::TerminalUnavailable,
                message: "Exact native chat is not hosted; explicit recovery is required".into(),
                retryable: false,
            }),
        );
        "The conversation was closed."
    } else {
        daemon.respond(
            &final_read,
            ServerResponse::ChatSnapshot {
                chat_id,
                snapshot: Some(chat::Snapshot {
                    generation: 2,
                    exited: true,
                    ..chat::Snapshot::default()
                }),
            },
        );
        assert!(matches!(
            next_line(&lines),
            api::ServerLine::Event(api::Event::Conversation(update)) if update.state.exited
        ));
        "The conversation ended."
    };
    assert_eq!(
        ending.recv_timeout(WAIT).unwrap(),
        (chat_id, Some(message.into())),
        "the bridge is told why after publishing any final state"
    );
    let api::ServerLine::Error { id: Some(7), error } = next_line(&lines) else {
        panic!("the interrupt is answered");
    };
    assert_eq!(error.code, api::ErrorCode::Unavailable);
}

#[test]
fn refreshing_a_read_checkpoint_keeps_pending_commands_on_the_same_watch() {
    refresh_read_checkpoint(false);
}

#[test]
fn a_retryable_read_keeps_the_checkpoint_for_the_next_retained_presentation() {
    refresh_read_checkpoint(true);
}

fn refresh_read_checkpoint(transient_read: bool) {
    use crate::notifications::{Event, Evidence, Kind, Notice, Snapshot};

    let (bridge, daemon) = UnixStream::pair().unwrap();
    daemon.set_read_timeout(Some(WAIT)).unwrap();
    let (outbox, lines) = Outbox::new();
    let mut daemon = Daemon(daemon);
    let session = session();
    let chat_id = session.id;
    let watch =
        start_scripted(1, session.clone(), bridge, proof(), outbox, None, |_, _| {}).unwrap();
    let attach = daemon.receive();
    daemon.respond(
        &attach,
        ServerResponse::ChatAttached {
            session,
            snapshot: chat::Snapshot::default(),
        },
    );
    let visibility = daemon.receive();
    daemon.respond(&visibility, ServerResponse::Accepted);
    daemon.approve_watch(chat_id);
    assert_eq!(next_line(&lines), api::ServerLine::Ok { id: 1, page: None });
    let api::ServerLine::Event(api::Event::Conversation(initial)) = next_line(&lines) else {
        panic!("the watch did not publish its reset");
    };
    let old_checkpoint = initial.read_checkpoint.unwrap().parse().unwrap();
    let initial_read = daemon.receive();
    assert!(matches!(
        initial_read.request,
        ClientRequest::ChatRead { .. }
    ));

    assert!(watch.send(Command::Chat {
        id: 2,
        command: chat::Command::Interrupt,
    }));
    let interrupt = daemon.receive();
    assert!(matches!(
        interrupt.request,
        ClientRequest::ChatCommand { .. }
    ));

    assert!(watch.send(Command::RefreshReadCheckpoint { id: 3 }));
    let refresh = daemon.receive();
    assert_eq!(refresh.request, ClientRequest::ReadNotifications);
    let notice = Notice {
        id: uuid::Uuid::new_v4(),
        event: Event::session(
            chat_id,
            Provider::Claude,
            "/fixture/project".into(),
            Evidence {
                key: "new".into(),
                kind: Kind::Waiting,
            },
        ),
        delivered: true,
        acknowledged: false,
        window: None,
    };
    daemon.respond(
        &refresh,
        ServerResponse::Notifications {
            snapshot: Snapshot {
                notices: vec![notice.clone()],
                diagnostic: None,
            },
        },
    );
    daemon.respond(
        &initial_read,
        ServerResponse::ChatSnapshot {
            chat_id,
            snapshot: None,
        },
    );
    let expected_read = ClientRequest::ChatRead {
        chat_id,
        after_generation: None,
    };
    let presentation = daemon.receive();
    assert_eq!(presentation.request, expected_read);
    assert!(
        lines.outgoing.try_recv().is_err(),
        "a journal receipt alone is not a presentation"
    );
    let (presentation, pending_command, command_id) = if transient_read {
        daemon.reply(
            &presentation,
            ServerMessage::Error(ProtocolError {
                code: ProtocolErrorCode::TerminalUnavailable,
                message: "Retained provider presentation is temporarily unavailable.".into(),
                retryable: true,
            }),
        );
        daemon.respond(&interrupt, ServerResponse::Accepted);
        assert_eq!(next_line(&lines), api::ServerLine::Ok { id: 2, page: None });

        // Commands can still use the watch while the actor waits to publish
        // its scheduled reread, and no extra presentation request races them.
        assert!(watch.send(Command::Chat {
            id: 6,
            command: chat::Command::Interrupt,
        }));
        let pending_command = daemon.receive();
        assert!(matches!(
            pending_command.request,
            ClientRequest::ChatCommand { .. }
        ));
        daemon.notify(ServerMessage::ChatChanged { chat_id });
        let presentation = daemon.receive();
        assert_eq!(presentation.request, expected_read);
        assert!(lines.outgoing.try_recv().is_err());
        (presentation, pending_command, 6)
    } else {
        (presentation, interrupt, 2)
    };
    daemon.respond(
        &presentation,
        ServerResponse::ChatSnapshot {
            chat_id,
            snapshot: Some(chat::Snapshot {
                generation: 2,
                title: Some("The refreshed conversation".into()),
                ..chat::Snapshot::default()
            }),
        },
    );
    assert_eq!(next_line(&lines), api::ServerLine::Ok { id: 3, page: None });
    let api::ServerLine::Event(api::Event::Conversation(fresh)) = next_line(&lines) else {
        panic!("the watch did not publish a refreshed presentation");
    };
    assert_eq!(fresh.session, chat_id.to_string());
    assert!(fresh.reset);
    assert_eq!(
        fresh.state.title.as_deref(),
        Some("The refreshed conversation")
    );
    let checkpoint = fresh.read_checkpoint.unwrap().parse().unwrap();
    assert_ne!(old_checkpoint, checkpoint);

    assert!(watch.send(Command::Read {
        id: 4,
        checkpoint: old_checkpoint
    }));
    assert!(matches!(
        next_line(&lines),
        api::ServerLine::Error { id: Some(4), error } if error.code == api::ErrorCode::NotWatching
    ));
    assert!(watch.send(Command::Read { id: 5, checkpoint }));
    let read = daemon.receive();
    assert!(matches!(
        read.request,
        ClientRequest::ChatAcknowledge { .. }
    ));
    daemon.respond(&read, ServerResponse::Accepted);
    let dismiss = daemon.receive();
    assert_eq!(
        dismiss.request,
        ClientRequest::AcknowledgeNotification { id: notice.id }
    );
    daemon.respond(&dismiss, ServerResponse::Accepted);
    assert_eq!(next_line(&lines), api::ServerLine::Ok { id: 5, page: None });

    daemon.respond(&pending_command, ServerResponse::Accepted);
    assert_eq!(
        next_line(&lines),
        api::ServerLine::Ok {
            id: command_id,
            page: None
        }
    );
}

#[test]
fn a_stopped_conversation_lookup_closes_its_socket_and_answers_pending_commands() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("daemon");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let (outbox, lines) = Outbox::new();
    let (ended, ending) = mpsc::channel();
    let chat_id = TerminalId::new();
    let shared = Arc::<details::Shared>::default();
    let watch = Watch::connect(
        1,
        chat_id,
        path,
        proof(),
        outbox,
        shared.clone(),
        move |chat, why| ended.send((chat, why)).unwrap(),
    )
    .unwrap();
    let (stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let mut daemon = Daemon(stream);
    let hello = daemon.receive();
    assert!(matches!(hello.request, ClientRequest::Connect { .. }));
    daemon.respond(
        &hello,
        ServerResponse::Connected {
            proof: proof(),
            resumed: Vec::new(),
        },
    );
    let list = daemon.receive();
    assert_eq!(list.request, ClientRequest::ChatList);

    assert!(watch.send(Command::Chat {
        id: 2,
        command: chat::Command::Interrupt
    }));
    assert!(watch.send(Command::Detail {
        id: 3,
        request: details::Request::Output {
            item: "command".into(),
            cursor: None,
        },
        slot: shared.reserve().unwrap(),
    }));
    assert!(shared.reserve().is_none());
    let upload_slot = Arc::new(());
    assert!(watch.send(Command::ImagePrompt {
        id: 4,
        text: String::new(),
        name: "photo.png".into(),
        png_base64: String::new(),
        mode: api::SendMode::Send,
        slot: upload_slot.clone(),
    }));
    drop(watch);
    assert!(
        read_frame::<ClientEnvelope>(&mut daemon.0)
            .unwrap()
            .is_none()
    );
    assert_eq!(ending.recv_timeout(WAIT).unwrap(), (chat_id, None));
    assert!(matches!(
        next_line(&lines),
        api::ServerLine::Error { id: Some(1), error } if error.code == api::ErrorCode::NotWatching
    ));
    for request in [2, 3, 4] {
        assert!(matches!(
            next_line(&lines),
            api::ServerLine::Error { id: Some(id), error }
                if id == request && error.code == api::ErrorCode::Unavailable
        ));
    }
    assert_eq!(Arc::strong_count(&upload_slot), 1);
    assert!(shared.reserve().is_some());
}

#[test]
fn a_watch_stopped_while_attaching_answers_once_and_sends_no_conversation() {
    let (bridge, daemon) = UnixStream::pair().unwrap();
    let (outbox, lines) = Outbox::new();
    let (ended, ending) = mpsc::channel();
    let watch = start_scripted(
        1,
        session(),
        bridge,
        proof(),
        outbox,
        None,
        move |chat, unfollowable| ended.send((chat, unfollowable)).unwrap(),
    )
    .unwrap();
    let mut daemon = Daemon(daemon);
    let attach = daemon.receive();
    assert!(matches!(attach.request, ClientRequest::ChatAttach { .. }));
    drop(watch);
    // The daemon sees detach before it replies to the pending attach.
    assert!(
        read_frame::<ClientEnvelope>(&mut daemon.0)
            .unwrap()
            .is_none()
    );

    let api::ServerLine::Error { id: Some(1), error } = next_line(&lines) else {
        panic!("the watch is answered with an error");
    };
    assert_eq!(error.code, api::ErrorCode::NotWatching);
    assert_eq!(ending.recv_timeout(WAIT).unwrap().1, None);
    assert!(
        lines
            .outgoing
            .recv_timeout(Duration::from_millis(200))
            .is_err(),
        "nothing more reaches the phone"
    );
}
