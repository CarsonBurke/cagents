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
    ended: impl FnOnce(TerminalId, Option<String>) + Send + 'static,
) -> std::io::Result<Watch> {
    Watch::start(
        id,
        session.id,
        outbox,
        move |stop| {
            stop.register(&stream)
                .map_err(|error| Failure::DaemonLost(error.to_string()))?;
            Ok(Opening {
                session,
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
    let watch = start_scripted(
        1,
        session.clone(),
        bridge,
        proof(),
        outbox,
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
    assert_eq!(next_line(&lines), api::ServerLine::Ok { id: 1 });
    assert!(matches!(
        next_line(&lines),
        api::ServerLine::Event(api::Event::Conversation(update)) if update.reset
    ));

    let read = daemon.receive();
    assert!(matches!(read.request, ClientRequest::ChatRead { .. }));
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
fn a_stopped_conversation_lookup_closes_its_socket_and_answers_pending_commands() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("daemon");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let (outbox, lines) = Outbox::new();
    let (ended, ending) = mpsc::channel();
    let chat_id = TerminalId::new();
    let watch = Watch::connect(1, chat_id, path, proof(), outbox, move |chat, why| {
        ended.send((chat, why)).unwrap()
    })
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
    assert!(matches!(
        next_line(&lines),
        api::ServerLine::Error { id: Some(2), error } if error.code == api::ErrorCode::Unavailable
    ));
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
