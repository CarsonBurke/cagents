use super::*;
#[cfg(feature = "ui")]
use crate::chat::coordination::{Received, Receipt};
use crate::agent_execution::{
    Authority, Operation, Reply,
    mailbox::{Inbox, MAX_MESSAGE_BYTES, Message, Outgoing, Reach, Sent, State, To, short},
};

/// How often a waiting caller asks the daemon for a reply.
const REPLY_POLL: Duration = Duration::from_millis(500);
/// Short enough for an agent harness's default command timeout, so the
/// wait ends before the harness kills it and keeps holding the reply.
const DEFAULT_WAIT: &str = "90";

/// A message given as an argument, in a file, or on standard input.
#[derive(Debug, clap::Args)]
pub struct MessageArgs {
    /// The message, which may begin with `-`; a lone `-` reads it from
    /// standard input.
    #[arg(value_name = "MESSAGE", allow_hyphen_values = true)]
    pub(super) text: Option<String>,
    /// Read the message from this file.
    #[arg(long, value_name = "FILE", conflicts_with = "text")]
    pub(super) file: Option<PathBuf>,
}

impl MessageArgs {
    fn read(self) -> Result<String, AppError> {
        agents::read_text(
            self.text,
            self.file,
            "message",
            "Give a message, --file FILE, or - to read standard input",
            MAX_MESSAGE_BYTES,
        )
    }
}

#[derive(Debug, clap::Args)]
pub struct SendArgs {
    /// The session: its ID, lineage or agent ID, or at least four characters
    /// of one; or `parent`, the session that started this one.
    #[arg(value_name = "SESSION")]
    pub(super) to: String,
    #[command(flatten)]
    pub(super) message: MessageArgs,
}

#[derive(Debug, clap::Args)]
pub struct AskArgs {
    #[command(flatten)]
    pub(super) send: SendArgs,
    /// How long to wait for the reply. A later reply arrives as a message.
    #[arg(long, default_value = DEFAULT_WAIT, value_parser = parse_wait)]
    timeout: f64,
}

#[derive(Debug, clap::Args)]
pub struct ReplyArgs {
    /// The message to answer, by its ID or a prefix of it.
    #[arg(value_name = "MESSAGE_ID")]
    pub(super) id: String,
    #[command(flatten)]
    pub(super) message: MessageArgs,
    /// Wait for the answer to this reply, as `ask` does.
    #[arg(long)]
    pub(super) wait: bool,
    #[arg(long, default_value = DEFAULT_WAIT, value_parser = parse_wait, requires = "wait")]
    timeout: f64,
}

#[derive(Debug, clap::Args)]
pub struct InboxArgs {
    /// List recent messages, including those already received, rather than
    /// only new ones.
    #[arg(long)]
    all: bool,
    /// Wait for a message if none is new.
    #[arg(long, conflicts_with = "all")]
    pub(super) wait: bool,
    #[arg(long, default_value = DEFAULT_WAIT, value_parser = parse_wait, requires = "wait")]
    timeout: f64,
}

fn parse_wait(value: &str) -> Result<f64, String> {
    let seconds = parse_positive_seconds(value)?;
    let limit = crate::agent_execution::mailbox::MAX_WAIT_MS / 1000;
    if seconds > limit as f64 {
        return Err(format!("timeout must not exceed {limit} seconds"));
    }
    Ok(seconds)
}

fn recipient(to: String) -> To {
    if to == "parent" {
        To::Parent
    } else {
        To::Session(to)
    }
}

pub(super) fn send(arguments: SendArgs) -> Result<CommandOutput, AppError> {
    let mut client = Mailbox::connect()?;
    let sent = client.send(Outgoing {
        to: recipient(arguments.to),
        body: arguments.message.read()?,
        wait_ms: None,
    })?;
    Ok(CommandOutput {
        command: "send",
        human: sent_line(&sent),
        data: json!({ "message": sent.message, "reach": sent.reach }),
    })
}

pub(super) fn ask(arguments: AskArgs) -> Result<CommandOutput, AppError> {
    let outgoing = Outgoing {
        to: recipient(arguments.send.to),
        body: arguments.send.message.read()?,
        wait_ms: None,
    };
    converse("ask", outgoing, arguments.timeout)
}

pub(super) fn reply(arguments: ReplyArgs) -> Result<CommandOutput, AppError> {
    let outgoing = Outgoing {
        to: To::Sender(arguments.id),
        body: arguments.message.read()?,
        wait_ms: None,
    };
    if arguments.wait {
        return converse("reply", outgoing, arguments.timeout);
    }
    let sent = Mailbox::connect()?.send(outgoing)?;
    Ok(CommandOutput {
        command: "reply",
        human: sent_line(&sent),
        data: json!({ "message": sent.message, "reach": sent.reach }),
    })
}

pub(super) fn inbox(arguments: InboxArgs) -> Result<CommandOutput, AppError> {
    let mut client = Mailbox::connect()?;
    let query = Inbox {
        all: arguments.all,
        replies_to: None,
    };
    let messages = if arguments.wait {
        client.wait(query, arguments.timeout)?.unwrap_or_default()
    } else {
        client.read(query)?
    };
    let human = if messages.is_empty() {
        if arguments.all {
            "No messages".to_owned()
        } else {
            "No new messages".to_owned()
        }
    } else {
        let mut lines: Vec<_> = messages.iter().map(message_lines).collect();
        lines.push("Answer one with: cagents reply <id> \"…\"".into());
        lines.join("\n\n")
    };
    Ok(CommandOutput {
        command: "inbox",
        human,
        data: json!({ "messages": messages }),
    })
}

/// Sends a message the sender waits on, then waits for its reply.
fn converse(
    command: &'static str,
    mut outgoing: Outgoing,
    timeout: f64,
) -> Result<CommandOutput, AppError> {
    let mut client = Mailbox::connect()?;
    outgoing.wait_ms = Some((timeout * 1000.0).ceil() as u64);
    let sent = client.send(outgoing)?;
    // The daemon holds no reply for an asker no one can answer yet.
    if sent.message.wait_until_ms.is_none() {
        return Ok(CommandOutput {
            command,
            human: format!(
                "{}\nNot waiting, since no one can answer before then; a later reply arrives as a message.",
                sent_line(&sent)
            ),
            data: json!({ "message": sent.message, "reach": sent.reach, "replies": [] }),
        });
    }
    let query = Inbox {
        all: false,
        replies_to: Some(sent.message.id),
    };
    match client.wait(query, timeout)? {
        Some(replies) => Ok(CommandOutput {
            command,
            human: replies
                .iter()
                .map(message_lines)
                .collect::<Vec<_>>()
                .join("\n\n"),
            data: json!({ "message": sent.message, "reach": sent.reach, "replies": replies }),
        }),
        None => Err(AppError::new(
            "timeout",
            format!(
                "{}\nNo reply within {timeout} s. A later reply arrives as a message; read it with `cagents inbox`.",
                sent_line(&sent)
            ),
        )),
    }
}

/// When a sent message reaches its recipient, as a sent line tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    /// A live conversation receives it as a prompt once it is idle.
    WhenIdle,
    /// A session in a terminal reads it with `cagents inbox`.
    Inbox,
    /// No session of it runs; the next to start receives it.
    NextSession,
    /// The user, outside every session, reads it with `cagents inbox`.
    User,
}

fn sent_line(sent: &Sent) -> String {
    let message = &sent.message;
    let what = if message.reply_to.is_some() {
        "Replied"
    } else {
        "Sent"
    };
    let arrival = match (sent.reach, message.to.lineage) {
        (_, None) => Arrival::User,
        (Reach::Conversation, _) => Arrival::WhenIdle,
        (Reach::Inbox, _) => Arrival::Inbox,
        (Reach::Offline, _) => Arrival::NextSession,
    };
    format!(
        "{what} {} to {}; {}",
        short(message.id),
        message.to.label(),
        arrival_phrase(arrival)
    )
}

/// How a sent line says a message will arrive, which is also how a
/// transcript reading the line back tells the four apart.
fn arrival_phrase(arrival: Arrival) -> &'static str {
    match arrival {
        Arrival::User => "the user reads it with `cagents inbox`",
        Arrival::WhenIdle => "it arrives as soon as that conversation is idle",
        Arrival::Inbox => "that session runs in a terminal and reads it with `cagents inbox`",
        Arrival::NextSession => "no session of it runs now; the next one to start receives it",
    }
}

/// The receipt in a line [`sent_line`] printed.
#[cfg(feature = "ui")]
pub(super) fn read_sent_line(line: &str) -> Option<Receipt> {
    let rest = line
        .strip_prefix("Sent ")
        .or_else(|| line.strip_prefix("Replied "))?;
    let (message, rest) = rest.split_once(" to ")?;
    let (recipient, arrival) = [
        Arrival::WhenIdle,
        Arrival::Inbox,
        Arrival::NextSession,
        Arrival::User,
    ]
    .into_iter()
    .find_map(|arrival| {
        let recipient = rest
            .strip_suffix(arrival_phrase(arrival))?
            .strip_suffix("; ")?;
        Some((recipient, arrival))
    })?;
    Some(Receipt {
        message: message.into(),
        recipient: recipient.into(),
        arrival,
    })
}

/// The messages in output that printed them with [`message_lines`].
#[cfg(feature = "ui")]
pub(super) fn read_messages(output: &str) -> Vec<Received> {
    let mut messages: Vec<Received> = Vec::new();
    let mut open = false;
    for line in output.lines() {
        if open && let Some(body) = line.strip_prefix("  ") {
            let message = messages.last_mut().expect("an open message");
            if !message.body.is_empty() {
                message.body.push('\n');
            }
            message.body.push_str(body);
            continue;
        }
        open = false;
        let Some((id, from)) = line.split_once("  from ") else {
            continue;
        };
        if id.is_empty() || id.contains(char::is_whitespace) {
            continue;
        }
        let from = from.strip_suffix(" (waiting for your reply)").unwrap_or(from);
        let from = from.split_once(", replying to ").map_or(from, |(from, _)| from);
        messages.push(Received {
            id: id.into(),
            from: from.into(),
            body: String::new(),
        });
        open = true;
    }
    messages
}

/// A message as `inbox`, `ask` and `agent wait` print it.
pub(super) fn message_lines(message: &Message) -> String {
    let mut heading = format!("{}  from {}", short(message.id), message.from.label());
    if let Some(original) = message.reply_to {
        heading.push_str(&format!(", replying to {}", short(original)));
    }
    if message
        .wait_until_ms
        .is_some_and(|until| until > crate::chat::now_ms().unwrap_or(0))
    {
        heading.push_str(" (waiting for your reply)");
    }
    let body: Vec<_> = message
        .body
        .trim_end()
        .lines()
        .map(|line| format!("  {}", printable(line)))
        .collect();
    let mut rendered = format!("{heading}\n{}", body.join("\n"));
    if message.state == State::Pending {
        rendered.push_str("\nReceipt not confirmed; this message may appear again.");
    }
    rendered
}

/// A line as a terminal may print it: control characters, which could
/// retitle or reprogram the terminal, are shown escaped.
fn printable(line: &str) -> String {
    let mut printed = String::with_capacity(line.len());
    for c in line.chars() {
        if c.is_control() && c != '\t' {
            printed.extend(c.escape_default());
        } else {
            printed.push(c);
        }
    }
    printed
}

/// One connection's worth of mailbox calls under the caller's authority.
struct Mailbox {
    authority: Authority,
    client: ControlClient,
}

impl Mailbox {
    fn connect() -> Result<Self, AppError> {
        let socket =
            terminal::ensure_daemon_ready().map_err(|e| AppError::new("daemon_start", e))?;
        let client =
            ControlClient::connect(&socket).map_err(|e| AppError::new("daemon_connection", e))?;
        Ok(Self {
            authority: Authority::from_environment(),
            client,
        })
    }

    fn send(&mut self, outgoing: Outgoing) -> Result<Sent, AppError> {
        outgoing
            .validate()
            .map_err(|e| AppError::new("message", e))?;
        match self
            .client
            .agent(self.authority.clone(), Operation::SendMessage(outgoing))
            .map_err(protocol_error)?
        {
            Reply::Sent(sent) => Ok(sent),
            _ => Err(AppError::new("message", "Unexpected send response")),
        }
    }

    fn read(&mut self, query: Inbox) -> Result<Vec<Message>, AppError> {
        let reply = self
            .client
            .agent(self.authority.clone(), Operation::Inbox(query))
            .map_err(protocol_error)?;
        inbox_messages(reply)
    }

    /// Reads until something matches `query`, or returns `None` once
    /// `timeout` seconds pass first. Every poll shares the wait's deadline,
    /// including time spent waiting for the daemon to answer.
    fn wait(&mut self, query: Inbox, timeout: f64) -> Result<Option<Vec<Message>>, AppError> {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);
        while Instant::now() < deadline {
            let reply = match self.client.agent_before(
                self.authority.clone(),
                Operation::Inbox(query.clone()),
                Some(deadline),
            ) {
                Ok(reply) => reply,
                Err(error) if is_timeout_error(&error) => return Ok(None),
                Err(error) => return Err(protocol_error(error)),
            };
            let messages = inbox_messages(reply)?;
            if !messages.is_empty() {
                return Ok(Some(messages));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(REPLY_POLL.min(remaining));
        }
        Ok(None)
    }
}

fn inbox_messages(reply: Reply) -> Result<Vec<Message>, AppError> {
    match reply {
        Reply::Inbox(messages) => Ok(messages),
        _ => Err(AppError::new("inbox", "Unexpected inbox response")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<TopCommand, clap::Error> {
        Cli::try_parse_from(std::iter::once("cagents").chain(arguments.iter().copied()))
            .map(|cli| cli.command)
    }

    #[test]
    fn messages_name_their_recipient_and_wait_within_bounds() {
        let TopCommand::Send(send) = parse(&["send", "parent", "-q is not a flag"]).unwrap() else {
            panic!("send")
        };
        assert_eq!(recipient(send.to), To::Parent);
        assert_eq!(send.message.text.as_deref(), Some("-q is not a flag"));
        let TopCommand::Ask(ask) = parse(&["ask", "1a2b", "Which branch?"]).unwrap() else {
            panic!("ask")
        };
        assert_eq!(recipient(ask.send.to), To::Session("1a2b".into()));
        assert_eq!(ask.timeout, 90.0);
        assert!(parse(&["ask", "1a2b", "hi", "--timeout", "3601"]).is_err());
        assert!(parse(&["reply", "7f3e", "hi", "--timeout", "5"]).is_err());
        let TopCommand::Reply(reply) =
            parse(&["reply", "7f3e", "--file", "/dev/null", "--wait"]).unwrap()
        else {
            panic!("reply")
        };
        assert!(reply.wait && reply.message.file.is_some());
        assert!(parse(&["inbox", "--all", "--wait"]).is_err());
        assert!(matches!(parse(&["inbox"]).unwrap(), TopCommand::Inbox(_)));
    }

    #[test]
    fn waiting_mailbox_bounds_stalled_poll_io_and_restores_socket_timeout() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let (release, released) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let request: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(matches!(request.request, ClientRequest::Connect { .. }));
            let proof = HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("mailbox-deadline"),
                boot_id: crate::model::BootId::new("boot"),
                host_incarnation: crate::model::HostIncarnation::new("incarnation"),
            };
            write_frame(
                &mut server_stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof,
                            resumed: Vec::new(),
                        },
                    },
                },
            )
            .unwrap();
            let request: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(matches!(
                request.request,
                ClientRequest::Agent {
                    operation: Operation::PeekInbox(_),
                    ..
                }
            ));
            // Hold the response until the caller returns; no timing-sensitive
            // sleep or late reply can make the deadline assertion pass.
            released.recv().unwrap();
        });
        let client = ControlClient::from_stream(client_stream).unwrap();
        client
            .stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut mailbox = Mailbox {
            authority: Authority::default(),
            client,
        };
        let result = mailbox.wait(
            Inbox {
                all: false,
                replies_to: None,
            },
            0.02,
        );
        release.send(()).unwrap();
        server.join().unwrap();
        assert!(
            matches!(result, Ok(None)),
            "a stalled poll must end as the requested wait timeout: {result:?}"
        );
        assert_eq!(
            mailbox.client.stream.read_timeout().unwrap(),
            Some(IO_TIMEOUT)
        );
    }

    #[test]
    fn a_printed_message_cannot_reprogram_the_terminal() {
        assert_eq!(
            printable("\u{1b}]52;c;aGk=\u{7}tab\there"),
            "\\u{1b}]52;c;aGk=\\u{7}tab\there"
        );
    }

    #[test]
    fn a_sent_line_says_how_the_message_arrives() {
        let message = Message {
            id: crate::model::MessageId::new(),
            from: crate::agent_execution::mailbox::Party::user(),
            to: crate::agent_execution::mailbox::Party {
                lineage: Some(crate::agent_execution::Lineage(TerminalId::new())),
                session: None,
                title: Some("Fix login".into()),
            },
            reply_to: None,
            body: "hi".into(),
            wait_until_ms: None,
            state: crate::agent_execution::mailbox::State::Pending,
            created_ms: 0,
        };
        let line = sent_line(&Sent {
            message: message.clone(),
            reach: Reach::Conversation,
        });
        assert!(line.starts_with(&format!("Sent {} to session ", short(message.id))));
        assert!(
            line.contains("\"Fix login\"; it arrives as soon as"),
            "{line}"
        );
        let mut to_user = message;
        to_user.to = crate::agent_execution::mailbox::Party::user();
        assert!(
            sent_line(&Sent {
                message: to_user,
                reach: Reach::Inbox,
            })
            .ends_with("to the user; the user reads it with `cagents inbox`")
        );
    }

    #[cfg(feature = "ui")]
    #[test]
    fn a_transcript_reads_back_what_the_cli_printed() {
        let mut message = Message {
            id: crate::model::MessageId::new(),
            from: crate::agent_execution::mailbox::Party {
                lineage: Some(crate::agent_execution::Lineage(TerminalId::new())),
                session: None,
                title: Some("Review; then merge".into()),
            },
            to: crate::agent_execution::mailbox::Party::user(),
            reply_to: None,
            body: "first\n\n  indented\nlast".into(),
            wait_until_ms: None,
            state: crate::agent_execution::mailbox::State::Delivered,
            created_ms: 0,
        };
        message.to = message.from.clone();
        for (reach, arrival) in [
            (Reach::Conversation, Arrival::WhenIdle),
            (Reach::Inbox, Arrival::Inbox),
            (Reach::Offline, Arrival::NextSession),
        ] {
            let line = sent_line(&Sent {
                message: message.clone(),
                reach,
            });
            let receipt = read_sent_line(&line).expect("reads its own line");
            assert_eq!(receipt.message, short(message.id));
            assert_eq!(receipt.recipient, message.to.label());
            assert_eq!(receipt.arrival, arrival);
        }
        let mut answer = message.clone();
        answer.reply_to = Some(crate::model::MessageId::new());
        let printed = [message_lines(&message), message_lines(&answer)].join("\n\n");
        let read = read_messages(&format!("{printed}\n\nAnswer one with: cagents reply <id> \"…\""));
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].id, short(message.id));
        assert_eq!(read[1].from, message.from.label());
        assert_eq!(read[1].body, "first\n\n  indented\nlast");
    }
}
