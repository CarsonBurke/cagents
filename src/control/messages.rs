use super::*;
use crate::agent_execution::{
    Authority, Operation, Reply,
    mailbox::{Inbox, MAX_MESSAGE_BYTES, Message, Outgoing, Reach, Sent, To, short},
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
    text: Option<String>,
    /// Read the message from this file.
    #[arg(long, value_name = "FILE", conflicts_with = "text")]
    file: Option<PathBuf>,
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
    to: String,
    #[command(flatten)]
    message: MessageArgs,
}

#[derive(Debug, clap::Args)]
pub struct AskArgs {
    #[command(flatten)]
    send: SendArgs,
    /// How long to wait for the reply. A later reply arrives as a message.
    #[arg(long, default_value = DEFAULT_WAIT, value_parser = parse_wait)]
    timeout: f64,
}

#[derive(Debug, clap::Args)]
pub struct ReplyArgs {
    /// The message to answer, by its ID or a prefix of it.
    #[arg(value_name = "MESSAGE_ID")]
    id: String,
    #[command(flatten)]
    message: MessageArgs,
    /// Wait for the answer to this reply, as `ask` does.
    #[arg(long)]
    wait: bool,
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
    wait: bool,
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

fn sent_line(sent: &Sent) -> String {
    let message = &sent.message;
    let what = if message.reply_to.is_some() {
        "Replied"
    } else {
        "Sent"
    };
    let delivery = match (sent.reach, message.to.lineage) {
        (_, None) => "the user reads it with `cagents inbox`",
        (Reach::Conversation, _) => "it arrives as soon as that conversation is idle",
        (Reach::Inbox, _) => "that session runs in a terminal and reads it with `cagents inbox`",
        (Reach::Offline, _) => "no session of it runs now; the next one to start receives it",
    };
    format!(
        "{what} {} to {}; {delivery}",
        short(message.id),
        message.to.label()
    )
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
    format!("{heading}\n{}", body.join("\n"))
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
        match self
            .client
            .agent(self.authority.clone(), Operation::Inbox(query))
            .map_err(protocol_error)?
        {
            Reply::Inbox(messages) => Ok(messages),
            _ => Err(AppError::new("inbox", "Unexpected inbox response")),
        }
    }

    /// Reads until something matches `query`, or returns `None` once
    /// `timeout` seconds pass first.
    fn wait(&mut self, query: Inbox, timeout: f64) -> Result<Option<Vec<Message>>, AppError> {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);
        loop {
            let messages = self.read(query.clone())?;
            if !messages.is_empty() {
                return Ok(Some(messages));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            thread::sleep(REPLY_POLL.min(remaining));
        }
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
}
