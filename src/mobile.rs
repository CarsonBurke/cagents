//! `cagents daemon bridge --mobile`: the mobile API over standard input and
//! output.
//!
//! A phone reaches this computer over SSH and runs this command, ideally as
//! the forced command of its key, so the key can do nothing else. The bridge
//! speaks the stable [`cagents_mobile_api`] to the phone and the daemon's own
//! protocol to the daemon, which it was built with. Because the bridge ships
//! with the daemon, the daemon's protocol can change freely: only this
//! translation has to keep the mobile API's promises.
//!
//! The bridge acts for the person, like the CLI in their own terminal, and
//! refuses to run in an agent's shell. It exposes only what the mobile API
//! names: watching sessions, notices and conversations, and messaging,
//! interrupting and answering conversations. Nothing reaches accounts, the
//! daemon's lifecycle or files.
//!
//! When the daemon stops, is replaced or is updated, the bridge says so and
//! exits; reconnecting starts the bridge that matches the daemon then running.

mod conversation;
mod translate;

use std::{
    collections::HashMap,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

use cagents_mobile_api as api;

use crate::{
    agent_execution::{Authority, Operation, Reply, directory::Scope},
    chat,
    control::ControlClient,
    model::TerminalId,
};

/// How often the bridge rereads the session list and notices while the
/// phone watches them. The daemon publishes neither as a change stream to a
/// client that isn't a window, so the bridge compares and sends only changes.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How many lines may wait for the phone before conversations stop reading
/// new publications. A slow link then gets fewer, larger updates rather than
/// a growing queue: each conversation sends what changed since what the
/// phone was last sent, however many publications that spans.
const MAX_BACKLOG: usize = 8;

/// Serve one phone until it disconnects or the daemon goes away.
pub fn serve_bridge() -> io::Result<()> {
    let socket = crate::device::bridge_socket()?;
    let (outbox, lines) = Outbox::new();
    let writer = thread::Builder::new()
        .name("mobile-write".into())
        .spawn(move || {
            write_lines(lines, io::stdout().lock());
            // Whatever ended the writing ends the bridge, including threads
            // still blocked on the daemon or on standard input.
            std::process::exit(0)
        })?;
    let mut input = io::stdin().lock();
    match serve(&socket, &mut input, &outbox) {
        Ok(()) => outbox.exit(),
        Err(message) => outbox.close(message),
    }
    // The writer ends the process once it has written everything before the
    // exit.
    let _ = writer.join();
    Ok(())
}

/// The phone's side of the session: negotiate, then answer each request.
/// Returns the reason the bridge stops, when the phone isn't what ended it.
fn serve(socket: &Path, input: &mut impl BufRead, outbox: &Outbox) -> Result<(), String> {
    let Some(hello) = read_hello(input, outbox)? else {
        return Ok(());
    };
    let Some(api_version) = negotiate(&hello) else {
        outbox.error(
            None,
            api::Error {
                code: api::ErrorCode::UnsupportedApi,
                message: format!(
                    "This computer's cagents speaks mobile API {}–{}; the app speaks {}–{}. Update {}.",
                    api::MIN_API_VERSION,
                    api::API_VERSION,
                    hello.api_min,
                    hello.api_max,
                    if hello.api_max < api::MIN_API_VERSION {
                        "the app"
                    } else {
                        "cagents on this computer"
                    }
                ),
            },
        );
        return Ok(());
    };
    let control = ControlClient::connect(socket)
        .map_err(|error| format!("Could not reach the computer's cagents daemon: {error}"))?;
    outbox.send(api::ServerLine::Welcome(api::Welcome {
        api: api_version,
        server_version: crate::version().into(),
        host: host_name(),
    }));
    let bridge = Bridge::new(socket.to_owned(), control, outbox.clone());
    loop {
        let line = match read_line(input) {
            Ok(Some(line)) => line,
            Ok(None) => return Ok(()),
            Err(TooLong) => {
                outbox.error(None, bad_request("The line is too long."));
                return Ok(());
            }
        };
        if let Some(stop) = bridge.handle(&line) {
            return Err(stop);
        }
    }
}

fn read_hello(input: &mut impl BufRead, outbox: &Outbox) -> Result<Option<api::Hello>, String> {
    let line = match read_line(input) {
        Ok(Some(line)) => line,
        Ok(None) => return Ok(None),
        Err(TooLong) => {
            outbox.error(None, bad_request("The line is too long."));
            return Ok(None);
        }
    };
    match serde_json::from_slice::<api::ClientLine>(&line) {
        Ok(api::ClientLine::Hello(hello)) => Ok(Some(hello)),
        Ok(api::ClientLine::Request(_)) | Err(_) => {
            outbox.error(None, bad_request("Expected hello first."));
            Ok(None)
        }
    }
}

/// The newest API version both sides speak.
fn negotiate(hello: &api::Hello) -> Option<u32> {
    let newest = hello.api_max.min(api::API_VERSION);
    (newest >= hello.api_min.max(api::MIN_API_VERSION)).then_some(newest)
}

fn host_name() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for its whole length; gethostname writes at
    // most that many bytes.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return None;
    }
    let length = buffer.iter().position(|byte| *byte == 0)?;
    std::str::from_utf8(&buffer[..length])
        .ok()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// A line longer than [`api::MAX_CLIENT_LINE_BYTES`].
struct TooLong;

/// One line without its newline, refusing one longer than the API allows
/// before buffering it whole. `None` once the phone's side has closed, which
/// a read error also means.
fn read_line(input: &mut impl BufRead) -> Result<Option<Vec<u8>>, TooLong> {
    let mut line = Vec::new();
    loop {
        let available = match input.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(None),
        };
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let (taken, done) = match available.iter().position(|byte| *byte == b'\n') {
            Some(end) => (end + 1, true),
            None => (available.len(), false),
        };
        line.extend_from_slice(&available[..taken]);
        input.consume(taken);
        if line.len() > api::MAX_CLIENT_LINE_BYTES + 1 {
            return Err(TooLong);
        }
        if done {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                line.clear();
                continue;
            }
            return Ok(Some(line));
        }
    }
}

enum Outgoing {
    Line(api::ServerLine),
    /// Flush what came before and end the process.
    Exit,
}

/// The writer's end of an [`Outbox`].
struct Lines {
    outgoing: Receiver<Outgoing>,
    backlog: Arc<AtomicUsize>,
}

/// Write each line to the phone in order until an exit, the phone going
/// away, or every sender being gone.
fn write_lines(lines: Lines, mut output: impl Write) {
    for outgoing in lines.outgoing {
        let Outgoing::Line(line) = outgoing else {
            break;
        };
        let written = serde_json::to_vec(&line).map(|mut bytes| {
            bytes.push(b'\n');
            output.write_all(&bytes).and_then(|()| output.flush())
        });
        lines.backlog.fetch_sub(1, Ordering::AcqRel);
        match written {
            Ok(Ok(())) => {}
            // The phone is gone; nothing more can reach it.
            Ok(Err(_)) => return,
            Err(error) => {
                eprintln!("cagents daemon bridge --mobile: could not encode a message: {error}");
            }
        }
    }
    let _ = output.flush();
}

/// Where every thread's messages to the phone go, in order.
#[derive(Clone)]
pub struct Outbox {
    outgoing: Sender<Outgoing>,
    /// Lines sent but not yet written.
    backlog: Arc<AtomicUsize>,
}

impl Outbox {
    fn new() -> (Self, Lines) {
        let (outgoing, received) = mpsc::channel();
        let backlog = Arc::new(AtomicUsize::new(0));
        (
            Self {
                outgoing,
                backlog: backlog.clone(),
            },
            Lines {
                outgoing: received,
                backlog,
            },
        )
    }

    fn send(&self, line: api::ServerLine) {
        self.backlog.fetch_add(1, Ordering::AcqRel);
        if self.outgoing.send(Outgoing::Line(line)).is_err() {
            self.backlog.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Whether the phone is behind enough that new publications should wait.
    pub fn congested(&self) -> bool {
        self.backlog.load(Ordering::Acquire) >= MAX_BACKLOG
    }

    /// Tell the phone why the bridge stops, then stop it.
    pub fn close(&self, message: String) {
        self.event(api::Event::Closing { message });
        self.exit();
    }

    fn exit(&self) {
        let _ = self.outgoing.send(Outgoing::Exit);
    }

    pub fn ok(&self, id: u64) {
        self.send(api::ServerLine::Ok { id });
    }

    pub fn error(&self, id: Option<u64>, error: api::Error) {
        self.send(api::ServerLine::Error { id, error });
    }

    pub fn event(&self, event: api::Event) {
        self.send(api::ServerLine::Event(event));
    }
}

fn bad_request(message: &str) -> api::Error {
    api::Error {
        code: api::ErrorCode::BadRequest,
        message: message.into(),
    }
}

/// What the phone watches and what it was last sent, shared with the thread
/// that polls the daemon.
#[derive(Default)]
struct Watched {
    sessions: bool,
    notifications: bool,
    sent_sessions: Option<Vec<api::Session>>,
    sent_notices: Option<Vec<api::Notice>>,
    /// Ask the poller to read now rather than at its next tick.
    poll_now: bool,
}

/// The conversations the phone watches, each with the generation that tells
/// a watch apart from one that replaced it.
type Conversations = Arc<Mutex<HashMap<TerminalId, (u64, conversation::Watch)>>>;

struct Bridge {
    socket: PathBuf,
    control: Arc<Mutex<ControlClient>>,
    watched: Arc<(Mutex<Watched>, Condvar)>,
    conversations: Conversations,
    next_watch: Mutex<u64>,
    outbox: Outbox,
}

impl Bridge {
    fn new(socket: PathBuf, control: ControlClient, outbox: Outbox) -> Self {
        let bridge = Self {
            socket,
            control: Arc::new(Mutex::new(control)),
            watched: Arc::default(),
            conversations: Arc::default(),
            next_watch: Mutex::new(0),
            outbox,
        };
        let control = bridge.control.clone();
        let watched = bridge.watched.clone();
        let conversations = bridge.conversations.clone();
        let outbox = bridge.outbox.clone();
        thread::Builder::new()
            .name("mobile-poll".into())
            .spawn(move || poll(&control, &watched, &conversations, &outbox))
            .expect("spawn mobile poll thread");
        bridge
    }

    /// Answer one line from the phone. Returns why the bridge must stop, if
    /// it must.
    fn handle(&self, line: &[u8]) -> Option<String> {
        let request = match parse_request(line) {
            Ok(request) => request,
            Err((id, error)) => {
                self.outbox.error(id, error);
                return None;
            }
        };
        let id = request.id;
        match self.call(id, request.call) {
            Ok(Answered::Now) => self.outbox.ok(id),
            Ok(Answered::Later) => {}
            Err(Failure::Refused(error)) => self.outbox.error(Some(id), error),
            Err(Failure::DaemonLost(message)) => {
                self.outbox.error(
                    Some(id),
                    api::Error {
                        code: api::ErrorCode::Unavailable,
                        message: message.clone(),
                    },
                );
                return Some(message);
            }
        }
        None
    }

    fn call(&self, id: u64, call: api::Call) -> Result<Answered, Failure> {
        match call {
            api::Call::Ping {} => Ok(Answered::Now),
            api::Call::WatchSessions {} => {
                self.watch(|watched| {
                    watched.sessions = true;
                    watched.sent_sessions = None;
                });
                Ok(Answered::Now)
            }
            api::Call::UnwatchSessions {} => {
                self.watch(|watched| {
                    watched.sessions = false;
                    watched.sent_sessions = None;
                });
                Ok(Answered::Now)
            }
            api::Call::WatchNotifications {} => {
                self.watch(|watched| {
                    watched.notifications = true;
                    watched.sent_notices = None;
                });
                Ok(Answered::Now)
            }
            api::Call::UnwatchNotifications {} => {
                self.watch(|watched| {
                    watched.notifications = false;
                    watched.sent_notices = None;
                });
                Ok(Answered::Now)
            }
            api::Call::AcknowledgeNotification { id: notice } => {
                let notice = uuid::Uuid::parse_str(&notice)
                    .map_err(|_| Failure::Refused(not_found("No such notice.")))?;
                self.control
                    .lock()
                    .unwrap()
                    .acknowledge_notification(notice)
                    .map_err(daemon_lost)?;
                // Reread so the phone sees the notice leave at once.
                self.watch(|_| {});
                Ok(Answered::Now)
            }
            api::Call::WatchConversation { session } => {
                let session = parse_session(&session)?;
                self.watch_conversation(id, session)?;
                Ok(Answered::Later)
            }
            api::Call::UnwatchConversation { session } => {
                let session = parse_session(&session)?;
                self.conversations.lock().unwrap().remove(&session);
                Ok(Answered::Now)
            }
            api::Call::LoadEarlier { session } => {
                self.command(id, &session, chat::Command::LoadEarlier)
            }
            api::Call::Send {
                session,
                text,
                mode,
            } => {
                if text.trim().is_empty() {
                    return Err(Failure::Refused(bad_request("The message is empty.")));
                }
                self.command(
                    id,
                    &session,
                    chat::Command::Prompt {
                        text,
                        attachments: Vec::new(),
                        mode: match mode {
                            api::SendMode::Send => chat::PromptMode::Send,
                            api::SendMode::Steer => chat::PromptMode::Steer,
                            api::SendMode::Queue => chat::PromptMode::Queue,
                            api::SendMode::Unknown => {
                                return Err(Failure::Refused(bad_request(
                                    "This computer's cagents doesn't know that way of sending; update it.",
                                )));
                            }
                        },
                    },
                )
            }
            api::Call::Interrupt { session } => {
                self.command(id, &session, chat::Command::Interrupt)
            }
            api::Call::Answer {
                session,
                request,
                action,
                answers,
            } => self.command(
                id,
                &session,
                chat::Command::Answer {
                    request_id: request,
                    action,
                    answers: answers
                        .into_iter()
                        .map(|answer| chat::Answer {
                            field: answer.field,
                            values: answer.values,
                        })
                        .collect(),
                },
            ),
        }
    }

    /// Change what the phone watches and have the poller read at once.
    fn watch(&self, change: impl FnOnce(&mut Watched)) {
        let (watched, wake) = &*self.watched;
        let mut watched = watched.lock().unwrap();
        change(&mut watched);
        watched.poll_now = true;
        wake.notify_all();
    }

    fn watch_conversation(&self, id: u64, session: TerminalId) -> Result<(), Failure> {
        let chats = self
            .control
            .lock()
            .unwrap()
            .list_chats()
            .map_err(daemon_lost)?;
        let Some(chat) = chats.into_iter().find(|chat| chat.id == session) else {
            return Err(Failure::Refused(api::Error {
                code: api::ErrorCode::NotWatchable,
                message: "Only a running native chat can be followed from the phone.".into(),
            }));
        };
        let (stream, proof) = ControlClient::connect(&self.socket)
            .map_err(daemon_lost)?
            .into_handshaken_stream();
        let generation = {
            let mut next = self.next_watch.lock().unwrap();
            *next += 1;
            *next
        };
        // A second watch of the same conversation starts it over. The first
        // ends before the second starts, so nothing it sends can follow the
        // second's reset.
        let mut watching = self.conversations.lock().unwrap();
        drop(watching.remove(&session));
        let conversations = Arc::downgrade(&self.conversations);
        let outbox = self.outbox.clone();
        let watch = conversation::Watch::start(
            id,
            chat,
            stream,
            proof,
            self.outbox.clone(),
            move |chat_id, unfollowable| {
                // Forget the watch, unless the phone has already replaced or
                // stopped it. Telling the phone under the same lock keeps the
                // notice ahead of any watch started after it.
                let Some(conversations) = conversations.upgrade() else {
                    return;
                };
                let mut conversations = conversations.lock().unwrap();
                if !conversations
                    .get(&chat_id)
                    .is_some_and(|(current, _)| *current == generation)
                {
                    return;
                }
                drop(conversations.remove(&chat_id));
                if let Some(message) = unfollowable {
                    outbox.event(api::Event::ConversationEnded {
                        session: chat_id.to_string(),
                        message,
                    });
                }
            },
        );
        watching.insert(session, (generation, watch));
        Ok(())
    }

    /// Send `command` to a watched conversation; its watch answers.
    fn command(&self, id: u64, session: &str, command: chat::Command) -> Result<Answered, Failure> {
        let session = parse_session(session)?;
        let conversations = self.conversations.lock().unwrap();
        let Some((_, watch)) = conversations.get(&session) else {
            return Err(Failure::Refused(api::Error {
                code: api::ErrorCode::NotWatching,
                message: "Open the conversation before acting on it.".into(),
            }));
        };
        if !watch.send(conversation::Command::Chat { id, command }) {
            return Err(Failure::Refused(api::Error {
                code: api::ErrorCode::NotWatching,
                message: "The conversation is no longer followed; open it again.".into(),
            }));
        }
        Ok(Answered::Later)
    }
}

enum Answered {
    /// Answer `ok` now.
    Now,
    /// Something else answers, once the daemon has.
    Later,
}

enum Failure {
    Refused(api::Error),
    /// The daemon connection failed; the bridge stops.
    DaemonLost(String),
}

fn daemon_lost(error: crate::control::ControlError) -> Failure {
    use crate::control::ControlError;
    match error {
        ControlError::Remote {
            code,
            message,
            retryable,
        } => Failure::Refused(conversation::refused(
            &crate::terminal::ipc::ProtocolError {
                code,
                message,
                retryable,
            },
        )),
        error => Failure::DaemonLost(format!(
            "Lost the connection to the computer's cagents daemon: {error}"
        )),
    }
}

fn not_found(message: &str) -> api::Error {
    api::Error {
        code: api::ErrorCode::NotFound,
        message: message.into(),
    }
}

fn parse_session(session: &str) -> Result<TerminalId, Failure> {
    session
        .parse()
        .map_err(|_| Failure::Refused(not_found("No such session.")))
}

/// A request, or the error that answers a line that isn't one: its `id`
/// when the line has one, so the phone can match the refusal.
fn parse_request(line: &[u8]) -> Result<api::Request, (Option<u64>, api::Error)> {
    let value: serde_json::Value =
        serde_json::from_slice(line).map_err(|_| (None, bad_request("The line isn't JSON.")))?;
    let id = value.get("id").and_then(serde_json::Value::as_u64);
    match serde_json::from_value::<api::ClientLine>(value.clone()) {
        Ok(api::ClientLine::Request(request)) => Ok(request),
        Ok(api::ClientLine::Hello(_)) => Err((id, bad_request("hello was already sent."))),
        Err(error) => {
            let op = value.get("op").and_then(serde_json::Value::as_str);
            let known = op.is_some_and(|op| api::Call::OPS.contains(&op));
            Err((
                id,
                match op {
                    Some(op) if !known => api::Error {
                        code: api::ErrorCode::UnknownOp,
                        message: format!("This computer's cagents doesn't know {op:?}."),
                    },
                    _ => bad_request(&format!("The request is malformed: {error}")),
                },
            ))
        }
    }
}

/// Reread what the phone watches every [`POLL_INTERVAL`], or at once when
/// asked, and send what changed.
fn poll(
    control: &Mutex<ControlClient>,
    watched: &(Mutex<Watched>, Condvar),
    conversations: &Conversations,
    outbox: &Outbox,
) {
    let (state, wake) = watched;
    loop {
        let (sessions, notifications) = {
            let mut state = state.lock().unwrap();
            if !state.poll_now {
                state = wake
                    .wait_timeout_while(state, POLL_INTERVAL, |state| !state.poll_now)
                    .unwrap()
                    .0;
            }
            state.poll_now = false;
            (state.sessions, state.notifications)
        };
        let following: Vec<(TerminalId, u64)> = conversations
            .lock()
            .unwrap()
            .iter()
            .map(|(chat, (generation, _))| (*chat, *generation))
            .collect();
        let read = (|| -> Result<_, crate::control::ControlError> {
            let mut control = control.lock().unwrap();
            let sessions = if sessions {
                match control.agent(Authority::default(), Operation::Sessions(Scope::All))? {
                    Reply::Sessions(listing) => {
                        Some(listing.sessions.iter().map(translate::session).collect())
                    }
                    _ => None,
                }
            } else {
                None
            };
            let notices = if notifications {
                Some(translate::notices(&control.read_notifications()?))
            } else {
                None
            };
            let chats = if following.is_empty() {
                None
            } else {
                Some(control.list_chats()?)
            };
            Ok((sessions, notices, chats))
        })();
        let (sessions, notices, chats): (
            Option<Vec<api::Session>>,
            Option<Vec<api::Notice>>,
            Option<Vec<chat::Session>>,
        ) = match read {
            Ok(read) => read,
            Err(error) => {
                outbox.close(format!(
                    "Lost the connection to the computer's cagents daemon: {error}"
                ));
                return;
            }
        };
        if let Some(chats) = chats {
            end_closed(conversations, &following, &chats, outbox);
        }
        let mut state = state.lock().unwrap();
        if let Some(sessions) = sessions
            && state.sessions
            && state.sent_sessions.as_ref() != Some(&sessions)
        {
            state.sent_sessions = Some(sessions.clone());
            outbox.event(api::Event::Sessions(api::Sessions { sessions }));
        }
        if let Some(notices) = notices
            && state.notifications
            && state.sent_notices.as_ref() != Some(&notices)
        {
            state.sent_notices = Some(notices.clone());
            outbox.event(api::Event::Notifications(api::Notifications { notices }));
        }
    }
}

/// End the watches of conversations that were closed elsewhere, such as on
/// the desktop. The daemon tells only the window that closed a chat, so a
/// watch would otherwise wait for changes that never come.
fn end_closed(
    conversations: &Conversations,
    following: &[(TerminalId, u64)],
    chats: &[chat::Session],
    outbox: &Outbox,
) {
    let mut watching = conversations.lock().unwrap();
    for (chat, generation) in following {
        if chats.iter().any(|open| open.id == *chat)
            || watching
                .get(chat)
                .is_none_or(|(current, _)| current != generation)
        {
            continue;
        }
        // Ending the watch first keeps anything it might still send from
        // following the notice.
        drop(watching.remove(chat));
        outbox.event(api::Event::ConversationEnded {
            session: chat.to_string(),
            message: "The conversation was closed.".into(),
        });
    }
}

#[cfg(test)]
#[path = "mobile_tests.rs"]
mod tests;
