//! Following one native chat for a phone.
//!
//! Each watch holds its own daemon connection, as a window's chat pane does:
//! attaching subscribes the connection to the chat, and the daemon then says
//! when the chat changed. The watch reads each change as a presentation
//! delta, applies it to the snapshot it holds, and sends the phone only what
//! differs from what the phone already has ([`Mirror`]).
//!
//! The connection's own thread owns the snapshot and is the only writer to
//! the socket; a second thread only reads frames into the same queue the
//! phone's commands arrive on, so requests and their replies stay in order.
//!
//! Every request the phone sends a watch is answered, including those the
//! watch couldn't finish before it ended.

use std::{
    collections::HashMap,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::Duration,
};

use cagents_mobile_api as api;

use super::{Outbox, translate::Mirror};
use crate::{
    chat,
    model::{HostSessionProof, TerminalId},
    terminal::ipc::{
        ClientEnvelope, ClientRequest, FrameError, PROTOCOL_VERSION, ProtocolError,
        ProtocolErrorCode, ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
};

/// What the phone asked of a watched conversation.
pub enum Command {
    /// A chat command, answered on the phone's request `id`.
    Chat { id: u64, command: chat::Command },
    /// End the watch.
    Stop,
}

/// How long attaching may take: the daemon may first load a long
/// transcript, as it does for the desktop.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(70);

/// How often a watch holding back for a slow phone checks whether it caught
/// up.
const CONGESTION_RECHECK: Duration = Duration::from_millis(100);

/// A running watch. Dropping it ends the watch, and nothing about the
/// conversation reaches the phone from it afterwards.
pub struct Watch {
    commands: Sender<Input>,
    current: Current,
}

/// Whether a watch still speaks for its conversation. Sending under its lock
/// means a watch that was replaced can't send after its replacement started.
#[derive(Clone)]
struct Current(Arc<Mutex<bool>>);

impl Current {
    fn send(&self, send: impl FnOnce()) -> bool {
        let current = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if *current {
            send();
        }
        *current
    }

    fn end(&self) {
        *self.0.lock().unwrap_or_else(|poison| poison.into_inner()) = false;
    }
}

impl Watch {
    /// Attach to `session` over `stream`, a daemon connection that has asked
    /// nothing since its handshake, and send the phone the whole conversation.
    /// Answers the phone's request `id` once the conversation is attached.
    /// `ended` runs when the watch ends, with why the conversation can't be
    /// followed when that is the reason; it tells the phone, unless the watch
    /// was replaced or stopped meanwhile.
    pub fn start(
        id: u64,
        session: chat::Session,
        stream: UnixStream,
        proof: HostSessionProof,
        outbox: Outbox,
        ended: impl FnOnce(TerminalId, Option<String>) + Send + 'static,
    ) -> Self {
        let (commands, inputs) = mpsc::channel();
        let feed = commands.clone();
        let current = Current(Arc::new(Mutex::new(true)));
        let speaking = current.clone();
        thread::Builder::new()
            .name("mobile-conversation".into())
            .spawn(move || {
                let chat_id = session.id;
                let mut connection = Connection {
                    stream,
                    proof,
                    next_request_id: 2,
                    chat_id,
                    outbox,
                    current: speaking,
                    inputs,
                    pending: HashMap::new(),
                };
                let end = connection.run(id, session, feed);
                let _ = connection.stream.shutdown(std::net::Shutdown::Both);
                let unfollowable = match &end {
                    End::Conversation(message) => Some(message.clone()),
                    End::Phone | End::Daemon(_) => None,
                };
                ended(chat_id, unfollowable);
                connection.abandon(&end);
                if let End::Daemon(message) = end {
                    connection.outbox.close(message);
                }
            })
            .expect("spawn mobile conversation thread");
        Self { commands, current }
    }

    /// Whether the watch was still running to take `command`.
    pub fn send(&self, command: Command) -> bool {
        self.commands.send(Input::Command(command)).is_ok()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.current.end();
        let _ = self.commands.send(Input::Command(Command::Stop));
    }
}

/// Why a watch ended.
enum End {
    /// The phone ended it.
    Phone,
    /// The conversation can't be followed any more.
    Conversation(String),
    /// The daemon went away; the bridge stops.
    Daemon(String),
}

enum Input {
    Frame(Box<Result<Option<ServerEnvelope>, FrameError>>),
    Command(Command),
}

/// What a daemon request in flight was for.
enum Pending {
    Read,
    Chat { id: u64 },
}

struct Connection {
    stream: UnixStream,
    proof: HostSessionProof,
    next_request_id: u64,
    chat_id: TerminalId,
    outbox: Outbox,
    current: Current,
    inputs: Receiver<Input>,
    /// What each daemon request in flight was for.
    pending: HashMap<u64, Pending>,
}

/// What a watch holds of its conversation between daemon messages.
struct Followed {
    session: String,
    snapshot: chat::Snapshot,
    mirror: Mirror,
    /// A change the held snapshot may predate.
    changed: bool,
    /// A publication read is in flight; one at a time keeps them in order.
    reading: bool,
}

impl Connection {
    /// Follow the chat until the phone, the conversation or the daemon ends
    /// the watch.
    fn run(&mut self, id: u64, session: chat::Session, feed: Sender<Input>) -> End {
        let snapshot = match self.attach(session) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.outbox.error(Some(id), error);
                return End::Phone;
            }
        };
        let mut followed = Followed {
            session: self.chat_id.to_string(),
            snapshot,
            mirror: Mirror::default(),
            // Attaching dropped the change notices that arrived during it,
            // so read once to begin with.
            changed: true,
            reading: false,
        };
        // The phone may have stopped or replaced this watch while it was
        // attaching; then neither the answer nor the conversation is wanted.
        let opened = self.current.send(|| {
            self.outbox.ok(id);
            if let Some(update) =
                followed
                    .mirror
                    .update(&followed.session, &followed.snapshot, true)
            {
                self.outbox
                    .event(api::Event::Conversation(Box::new(update)));
            }
        });
        if !opened {
            self.outbox.error(
                Some(id),
                api_error(
                    api::ErrorCode::NotWatching,
                    "The conversation was closed on the phone before it opened.",
                ),
            );
            return End::Phone;
        }
        // The watch has shown its first snapshot. A watch cancelled during
        // attachment must leave the conversation's attention unread.
        if let Err(error) = self.request(ClientRequest::AcknowledgeSessionNotifications {
            session: self.chat_id,
        }) {
            return End::Daemon(lost(&error));
        }
        // From here on the reader thread waits for frames for as long as the
        // conversation is followed.
        let reader = self
            .stream
            .set_read_timeout(None)
            .and_then(|()| self.stream.try_clone());
        let mut reader = match reader {
            Ok(stream) => stream,
            Err(error) => {
                return End::Conversation(format!("Could not follow the conversation: {error}"));
            }
        };
        thread::Builder::new()
            .name("mobile-conversation-read".into())
            .spawn(move || {
                loop {
                    let frame = read_frame::<ServerEnvelope>(&mut reader);
                    let last = !matches!(frame, Ok(Some(_)));
                    if feed.send(Input::Frame(Box::new(frame))).is_err() || last {
                        return;
                    }
                }
            })
            .expect("spawn mobile conversation reader");
        loop {
            // A phone that is behind gets the conversation as it stands once
            // it catches up, rather than every publication on the way.
            let held_back = followed.changed && !followed.reading && self.outbox.congested();
            if followed.changed && !followed.reading && !held_back {
                let after_generation =
                    (followed.snapshot.generation != 0).then_some(followed.snapshot.generation);
                match self.request(ClientRequest::ChatRead {
                    chat_id: self.chat_id,
                    after_generation,
                }) {
                    Ok(request_id) => {
                        self.pending.insert(request_id, Pending::Read);
                        followed.reading = true;
                        followed.changed = false;
                    }
                    Err(error) => return End::Daemon(lost(&error)),
                }
            }
            let input = if held_back {
                match self.inputs.recv_timeout(CONGESTION_RECHECK) {
                    Ok(input) => Ok(input),
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => Err(()),
                }
            } else {
                self.inputs.recv().map_err(|_| ())
            };
            let handled = match input {
                Err(()) | Ok(Input::Command(Command::Stop)) => return End::Phone,
                Ok(Input::Command(Command::Chat { id, command })) => {
                    self.command(&mut followed, id, command)
                }
                Ok(Input::Frame(frame)) => match *frame {
                    Ok(Some(envelope)) => self.envelope(&mut followed, envelope),
                    Ok(None) => Err(End::Daemon("The computer's cagents daemon stopped.".into())),
                    Err(error) => Err(End::Daemon(lost(&error))),
                },
            };
            if let Err(end) = handled {
                return end;
            }
        }
    }

    /// Send the phone what changed since it was last sent anything.
    fn publish(&self, followed: &mut Followed) {
        self.current.send(|| {
            if let Some(update) =
                followed
                    .mirror
                    .update(&followed.session, &followed.snapshot, false)
            {
                self.outbox
                    .event(api::Event::Conversation(Box::new(update)));
            }
        });
    }

    /// Answer every phone request the watch is ending without finishing:
    /// those sent to the daemon and those still waiting behind the end.
    fn abandon(&mut self, end: &End) {
        let message = match end {
            End::Phone => "The conversation stopped being followed before the computer answered.",
            End::Conversation(_) => "The conversation ended before the computer answered.",
            End::Daemon(_) => "The computer's cagents daemon stopped before it answered.",
        };
        let pending = std::mem::take(&mut self.pending);
        let waiting = self.inputs.try_iter().filter_map(|input| match input {
            Input::Command(Command::Chat { id, .. }) => Some(id),
            _ => None,
        });
        for id in pending
            .into_values()
            .filter_map(|pending| match pending {
                Pending::Chat { id } => Some(id),
                Pending::Read => None,
            })
            .chain(waiting)
        {
            self.outbox
                .error(Some(id), api_error(api::ErrorCode::Unavailable, message));
        }
    }

    fn command(
        &mut self,
        followed: &mut Followed,
        id: u64,
        command: chat::Command,
    ) -> Result<(), End> {
        let command = match answerable(&followed.snapshot, command) {
            Ok(command) => command,
            Err(error) => {
                self.outbox.error(Some(id), error);
                return Ok(());
            }
        };
        match self.request(ClientRequest::ChatCommand {
            chat_id: self.chat_id,
            command,
        }) {
            Ok(request_id) => {
                self.pending.insert(request_id, Pending::Chat { id });
                Ok(())
            }
            Err(error) => {
                self.outbox.error(Some(id), unavailable(&error));
                Err(End::Daemon(lost(&error)))
            }
        }
    }

    fn envelope(&mut self, followed: &mut Followed, envelope: ServerEnvelope) -> Result<(), End> {
        if envelope.version != PROTOCOL_VERSION || envelope.proof != self.proof {
            return Err(End::Daemon(
                "The computer's cagents daemon was replaced.".into(),
            ));
        }
        let Some(request_id) = envelope.request_id else {
            return match envelope.message {
                ServerMessage::ChatChanged { chat_id } if chat_id == self.chat_id => {
                    followed.changed = true;
                    Ok(())
                }
                ServerMessage::HostShuttingDown => Err(End::Daemon(
                    "The computer's cagents daemon is stopping.".into(),
                )),
                // The daemon is dropping this connection, such as for falling
                // too far behind.
                ServerMessage::Error(error) => Err(End::Conversation(error.message)),
                // Activity, usage, model and address edges arrive in the next
                // publication the phone is sent.
                _ => Ok(()),
            };
        };
        match self.pending.remove(&request_id) {
            Some(Pending::Read) => {
                followed.reading = false;
                match envelope.message {
                    ServerMessage::Response {
                        response: ServerResponse::ChatSnapshot { snapshot, .. },
                    } => {
                        if let Some(snapshot) = snapshot {
                            followed.snapshot = snapshot;
                        }
                    }
                    ServerMessage::Response {
                        response: ServerResponse::ChatUpdate { update, .. },
                    } => match update.apply(&followed.snapshot) {
                        Ok(snapshot) => followed.snapshot = snapshot,
                        // A delta that doesn't fit what is held: read the
                        // whole snapshot again.
                        Err(_) => {
                            followed.snapshot.generation = 0;
                            followed.changed = true;
                        }
                    },
                    ServerMessage::Error(error)
                        if error.code == ProtocolErrorCode::TerminalNotFound =>
                    {
                        return Err(End::Conversation(error.message));
                    }
                    // A read can fail and succeed next time, as when the
                    // provider is busy. As the desktop does, keep what is
                    // held, read everything on the next change, and show
                    // the failure unless the daemon says it will pass.
                    ServerMessage::Error(error) => {
                        followed.snapshot.generation = 0;
                        if !error.retryable {
                            followed.snapshot.error = Some(error.message);
                        }
                    }
                    _ => {
                        return Err(End::Conversation(
                            "The daemon answered a conversation read unexpectedly.".into(),
                        ));
                    }
                }
                self.publish(followed);
            }
            Some(Pending::Chat { id }) => match envelope.message {
                ServerMessage::Response { .. } => self.outbox.ok(id),
                ServerMessage::Error(error) => self.outbox.error(Some(id), refused(&error)),
                _ => self
                    .outbox
                    .error(Some(id), internal("The daemon answered unexpectedly.")),
            },
            None => {}
        }
        Ok(())
    }

    /// Attach and make the chat visible, so the daemon publishes its full
    /// presentation to this connection.
    fn attach(&mut self, session: chat::Session) -> Result<chat::Snapshot, api::Error> {
        self.stream
            .set_read_timeout(Some(ATTACH_TIMEOUT))
            .map_err(|error| unavailable(&error))?;
        let snapshot = match self.call(ClientRequest::ChatAttach { session })? {
            ServerResponse::ChatAttached { snapshot, .. } => snapshot,
            _ => return Err(internal("The daemon answered an attach unexpectedly.")),
        };
        match self.call(ClientRequest::ChatVisibility {
            chat_id: self.chat_id,
            visible: true,
        })? {
            ServerResponse::Accepted => {}
            _ => return Err(internal("The daemon answered unexpectedly.")),
        }
        Ok(snapshot)
    }

    /// Send `request` and wait for its answer. Only for the setup before the
    /// reader starts; the chat's own notifications that arrive meanwhile are
    /// covered by the publication read after it.
    fn call(&mut self, request: ClientRequest) -> Result<ServerResponse, api::Error> {
        let request_id = self.request(request).map_err(|error| unavailable(&error))?;
        loop {
            let envelope = read_frame::<ServerEnvelope>(&mut self.stream)
                .map_err(|error| unavailable(&error))?
                .ok_or_else(|| {
                    api_error(
                        api::ErrorCode::Unavailable,
                        "The computer's cagents daemon stopped.",
                    )
                })?;
            if envelope.proof != self.proof {
                return Err(api_error(
                    api::ErrorCode::Unavailable,
                    "The computer's cagents daemon was replaced.",
                ));
            }
            if envelope.request_id != Some(request_id) {
                continue;
            }
            return match envelope.message {
                ServerMessage::Response { response } => Ok(response),
                ServerMessage::Error(error) => Err(refused(&error)),
                _ => Err(internal("The daemon answered unexpectedly.")),
            };
        }
    }

    fn request(&mut self, request: ClientRequest) -> Result<u64, FrameError> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        write_frame(
            &mut self.stream,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id,
                proof: Some(self.proof.clone()),
                request,
            },
        )?;
        Ok(request_id)
    }
}

/// `command`, with an answer completed as the desktop completes it: the
/// action must be one the request still offers, and every field is
/// answered, a field the phone left out with no values.
pub(super) fn answerable(
    snapshot: &chat::Snapshot,
    command: chat::Command,
) -> Result<chat::Command, api::Error> {
    let chat::Command::Answer {
        request_id,
        action,
        answers,
    } = command
    else {
        return Ok(command);
    };
    let request = snapshot
        .requests
        .iter()
        .find(|request| request.id == request_id)
        .ok_or_else(|| {
            api_error(
                api::ErrorCode::NotFound,
                "That request was already answered or withdrawn.",
            )
        })?;
    if !request.actions.iter().any(|choice| choice.id == action) {
        return Err(api_error(
            api::ErrorCode::BadRequest,
            "That request doesn't offer this choice.",
        ));
    }
    if let Some(unknown) = answers
        .iter()
        .find(|answer| !request.fields.iter().any(|field| field.id == answer.field))
    {
        return Err(api_error(
            api::ErrorCode::BadRequest,
            &format!("That request has no field {:?}.", unknown.field),
        ));
    }
    let answers = request
        .fields
        .iter()
        .map(|field| chat::Answer {
            field: field.id.clone(),
            values: answers
                .iter()
                .find(|answer| answer.field == field.id)
                .map(|answer| answer.values.clone())
                .unwrap_or_default(),
        })
        .collect();
    Ok(chat::Command::Answer {
        request_id,
        action,
        answers,
    })
}

/// How a refusal from the daemon reads on the phone.
pub fn refused(error: &ProtocolError) -> api::Error {
    let code = match error.code {
        ProtocolErrorCode::PermissionDenied
        | ProtocolErrorCode::NotAdmitted
        | ProtocolErrorCode::AccountRemoved => api::ErrorCode::NotAllowed,
        ProtocolErrorCode::TerminalNotFound => api::ErrorCode::NotFound,
        ProtocolErrorCode::InvalidRequest => api::ErrorCode::BadRequest,
        ProtocolErrorCode::Internal
        | ProtocolErrorCode::UnsupportedVersion
        | ProtocolErrorCode::HandshakeRequired
        | ProtocolErrorCode::AlreadyConnected
        | ProtocolErrorCode::ProofMismatch => api::ErrorCode::Internal,
        ProtocolErrorCode::TerminalAlreadyExists
        | ProtocolErrorCode::TerminalUnavailable
        | ProtocolErrorCode::QueueFull
        | ProtocolErrorCode::ViewConflict
        | ProtocolErrorCode::ChatCapacity
        | ProtocolErrorCode::StoreBusy => api::ErrorCode::Unavailable,
    };
    api_error(code, &error.message)
}

fn unavailable(error: &dyn std::fmt::Display) -> api::Error {
    api_error(
        api::ErrorCode::Unavailable,
        &format!("Lost the conversation: {error}"),
    )
}

fn lost(error: &dyn std::fmt::Display) -> String {
    format!("Lost the conversation: {error}")
}

fn internal(message: &str) -> api::Error {
    api_error(api::ErrorCode::Internal, message)
}

fn api_error(code: api::ErrorCode, message: &str) -> api::Error {
    api::Error {
        code,
        message: message.into(),
    }
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
#[path = "conversation_tests.rs"]
mod tests;
