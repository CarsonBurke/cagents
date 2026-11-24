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
}

fn next_line(lines: &Lines) -> api::ServerLine {
    match lines
        .outgoing
        .recv_timeout(WAIT)
        .expect("a line for the phone")
    {
        Outgoing::Line(line) => line,
        Outgoing::Exit => panic!("the bridge exited"),
    }
}

#[test]
fn a_conversation_that_goes_away_ends_its_watch_and_answers_what_was_in_flight() {
    let (bridge, daemon) = UnixStream::pair().unwrap();
    let (outbox, lines) = Outbox::new();
    let (ended, ending) = mpsc::channel();
    let session = session();
    let chat_id = session.id;
    let watch = Watch::start(
        1,
        session.clone(),
        bridge,
        proof(),
        outbox,
        move |chat, unfollowable| ended.send((chat, unfollowable)).unwrap(),
    );
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
    assert_eq!(next_line(&lines), api::ServerLine::Ok { id: 1 });
    assert!(matches!(
        next_line(&lines),
        api::ServerLine::Event(api::Event::Conversation(update)) if update.reset
    ));
    let acknowledgement = daemon.receive();
    assert!(matches!(
        acknowledgement.request,
        ClientRequest::AcknowledgeSessionNotifications { session } if session == chat_id
    ));
    daemon.respond(&acknowledgement, ServerResponse::Accepted);

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
    // The chat is gone before either is answered.
    daemon.reply(
        &read,
        ServerMessage::Error(ProtocolError {
            code: ProtocolErrorCode::TerminalNotFound,
            message: "The conversation is gone.".into(),
            retryable: false,
        }),
    );

    assert_eq!(
        ending.recv_timeout(WAIT).unwrap(),
        (chat_id, Some("The conversation is gone.".into())),
        "the bridge is told why, to tell the phone"
    );
    let api::ServerLine::Error { id: Some(7), error } = next_line(&lines) else {
        panic!("the interrupt is answered");
    };
    assert_eq!(error.code, api::ErrorCode::Unavailable);
}

#[test]
fn a_watch_stopped_while_attaching_answers_once_and_sends_no_conversation() {
    let (bridge, daemon) = UnixStream::pair().unwrap();
    let (outbox, lines) = Outbox::new();
    let (ended, ending) = mpsc::channel();
    let session = session();
    let watch = Watch::start(
        1,
        session.clone(),
        bridge,
        proof(),
        outbox,
        move |chat, unfollowable| ended.send((chat, unfollowable)).unwrap(),
    );
    let mut daemon = Daemon(daemon);
    let attach = daemon.receive();
    drop(watch);
    daemon.respond(
        &attach,
        ServerResponse::ChatAttached {
            session,
            snapshot: chat::Snapshot::default(),
        },
    );
    let visibility = daemon.receive();
    daemon.respond(&visibility, ServerResponse::Accepted);

    let api::ServerLine::Error { id: Some(1), error } = next_line(&lines) else {
        panic!("the watch is answered with an error");
    };
    assert_eq!(error.code, api::ErrorCode::NotWatching);
    assert_eq!(ending.recv_timeout(WAIT).unwrap().1, None);
    assert!(
        read_frame::<ClientEnvelope>(&mut daemon.0)
            .unwrap()
            .is_none()
    );
    assert!(
        lines
            .outgoing
            .recv_timeout(Duration::from_millis(200))
            .is_err(),
        "nothing more reaches the phone"
    );
}
