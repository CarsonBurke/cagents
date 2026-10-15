//! Messages between sessions.
//!
//! A session that needs another's attention, or the user at their own CLI,
//! leaves a message for that session's lineage, so it still arrives after the
//! session is relaunched or recovered. The daemon hands a message to a live
//! conversation as a prompt once it is idle; a session that runs in a terminal
//! reads its messages with `cagents inbox`.
//!
//! A delivered message is ordinary prompt text, framed so that the agent knows
//! who sent it and how to answer. The daemon seals the frame, so a window shows
//! a message as one only when the daemon wrote it, never because a prompt was
//! made to look like one.

use serde::{Deserialize, Serialize};

use super::Lineage;
use crate::model::{MessageId, TerminalId};

pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// How many undelivered messages one lineage may hold, so that two agents
/// answering each other in a loop fill a queue rather than the disk.
pub const MAX_PENDING: usize = 64;
/// How long a message is kept after it was sent, delivered or not.
pub const RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// The longest an asker may keep the reply to itself.
pub const MAX_WAIT_MS: u64 = 60 * 60 * 1000;
/// How much of a sender's title a frame repeats.
const TITLE_CHARS: usize = 80;

/// Whom a message is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum To {
    /// A live session, by its ID, its lineage, its delegated agent, or a
    /// unique prefix of any of them.
    Session(String),
    /// The session that started the caller as its agent.
    Parent,
    /// Whoever sent the message the caller received with this ID or a unique
    /// prefix of it; the new message replies to it.
    Sender(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outgoing {
    pub to: To,
    pub body: String,
    /// How long the sender waits for the reply itself. Until then the reply is
    /// held for it rather than delivered to its conversation.
    pub wait_ms: Option<u64>,
}

impl Outgoing {
    pub fn validate(&self) -> Result<(), String> {
        if self.body.trim().is_empty() || self.body.len() > MAX_MESSAGE_BYTES {
            return Err(format!(
                "A message must contain text and be at most {MAX_MESSAGE_BYTES} bytes"
            ));
        }
        if self.body.contains('\0') {
            return Err("A message cannot contain a NUL byte".into());
        }
        if self
            .wait_ms
            .is_some_and(|wait| wait == 0 || wait > MAX_WAIT_MS)
        {
            return Err(format!(
                "A sender can wait between 1 ms and {} minutes for a reply",
                MAX_WAIT_MS / 60_000
            ));
        }
        match &self.to {
            To::Session(handle) | To::Sender(handle)
                if handle.is_empty()
                    || handle.len() > 64
                    || handle.chars().any(char::is_control) =>
            {
                Err("Name the recipient by an ID or a prefix of one".into())
            }
            _ => Ok(()),
        }
    }
}

/// One end of a message: a session's lineage, or the user's own CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Party {
    /// Absent for the user, at a CLI outside every cagents session.
    pub lineage: Option<Lineage>,
    /// The session that sent, or the one that received, if known.
    pub session: Option<TerminalId>,
    pub title: Option<String>,
}

impl Party {
    pub fn user() -> Self {
        Self {
            lineage: None,
            session: None,
            title: None,
        }
    }

    /// The short ID a person or an agent would type back.
    pub fn short(&self) -> Option<String> {
        self.session
            .or(self.lineage.map(|lineage| lineage.0))
            .map(short)
    }

    /// How a list or a frame names this party.
    pub fn label(&self) -> String {
        match (self.short(), self.title.as_deref().map(title)) {
            (None, _) => "the user".into(),
            (Some(id), Some(title)) if !title.is_empty() => format!("session {id} \"{title}\""),
            (Some(id), _) => format!("session {id}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Not yet given to the recipient.
    Pending,
    /// Given to a live conversation as a prompt.
    Delivered,
    /// Read with `cagents inbox`, or by the asker that waited for it.
    Read,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub from: Party,
    pub to: Party,
    pub reply_to: Option<MessageId>,
    pub body: String,
    /// Until when, in milliseconds since the epoch, the sender waits for the
    /// reply itself.
    pub wait_until_ms: Option<u64>,
    pub state: State,
    pub created_ms: u64,
}

/// How a sent message will reach its recipient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reach {
    /// A live conversation receives it as a prompt once it is idle.
    Conversation,
    /// A session in a terminal reads it with `cagents inbox`.
    Inbox,
    /// No session of the lineage runs; the next one to start receives it.
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sent {
    pub message: Message,
    pub reach: Reach,
}

/// A read of the caller's messages. Each message it returns that its recipient
/// had not yet received is marked read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inbox {
    /// Recent messages whether received or not, rather than only new ones.
    pub all: bool,
    /// Only replies to this message the caller sent.
    pub replies_to: Option<MessageId>,
}

/// Exact messages received by an inbox client. Acknowledgement happens only
/// after the response has reached it, so a disconnected poll loses no mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub ids: Vec<MessageId>,
}

impl Receipt {
    pub fn validate(&self) -> Result<(), String> {
        if self.ids.len() > MAX_PENDING {
            return Err(format!(
                "A mailbox receipt may contain at most {MAX_PENDING} IDs"
            ));
        }
        Ok(())
    }
}

pub fn short(id: impl std::fmt::Display) -> String {
    id.to_string().chars().take(8).collect()
}

/// A title as a frame quotes it: one line, without the characters that
/// delimit the frame or invisibly reorder the text around it, and not so
/// long that it buries the message.
fn title(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .filter(|c| {
            !matches!(
                c,
                '"' | '['
                    | ']'
                    | ';'
                    | '\u{00AD}'
                    | '\u{061C}'
                    | '\u{200B}'..='\u{200F}'
                    | '\u{202A}'..='\u{202E}'
                    | '\u{2060}'..='\u{2069}'
                    | '\u{FEFF}'
            )
        })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    match cleaned.char_indices().nth(TITLE_CHARS) {
        Some((end, _)) => format!("{}…", &cleaned[..end]),
        None => cleaned,
    }
}

const OPENING: &str = "[cagents ";
const SEAL: &str = " · seal ";

/// The prompt a message becomes at `now`, up to its seal. The first line says
/// who sent it, the last how to answer; the body between them is the sender's
/// own.
pub fn frame(message: &Message, now: u64) -> String {
    let kind = if message.reply_to.is_some() {
        "reply"
    } else if message.wait_until_ms.is_some() {
        "question"
    } else {
        "message"
    };
    let mut heading = format!("{OPENING}{kind} from {}", message.from.label());
    if let Some(original) = message.reply_to {
        heading.push_str(&format!(" to your message {}", short(original)));
    }
    // A question that waited out a busy recipient still asks, but no one
    // waits for the answer any more.
    if message.wait_until_ms.is_some_and(|until| until > now) {
        heading.push_str("; they are waiting for your reply");
    }
    format!(
        "{heading}]\n{}\n[Reply: cagents reply {} \"…\"",
        message.body.trim_end(),
        short(message.id)
    )
}

/// A sealed frame, opened for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// The sender's title, `agent` and its short ID when it has none, or
    /// `you` for the user.
    pub from: String,
    /// A titled sender's short session ID, which tells two of one title
    /// apart; otherwise empty.
    pub sender: String,
    pub summary: String,
    pub body: String,
}

/// Reads `text` as a frame whose seal `verify` accepts for everything before
/// it. Anything else, however much it resembles a frame, is not a message.
pub fn open(text: &str, verify: impl FnOnce(&str, &str) -> bool) -> Option<Opened> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let (framed, seal) = text.rsplit_once(SEAL)?;
    let seal = seal.strip_suffix(']')?;
    if seal.contains(['\n', ' ']) || !verify(framed, seal) {
        return None;
    }
    let (heading, rest) = framed.strip_prefix(OPENING)?.split_once("]\n")?;
    let (body, _) = rest.rsplit_once("\n[Reply: ")?;
    let (kind, rest) = heading.split_once(" from ")?;
    // A title holds no `;`, so the first one ends the party.
    let party = rest.split_once("; ").map_or(rest, |(party, _)| party);
    // Only a reply names an original, and it does so after any title.
    let (party, original) = match kind {
        "reply" => {
            let (party, original) = party.rsplit_once(" to your message ")?;
            (party, Some(original))
        }
        _ => (party, None),
    };
    let (from, sender) = if party == "the user" {
        ("you".to_owned(), String::new())
    } else {
        let party = party.strip_prefix("session ")?;
        // An untitled sender is named by its ID, which a window would
        // otherwise replace with the summary.
        match party.split_once(' ') {
            Some((id, title)) => (title.trim_matches('"').to_owned(), id.to_owned()),
            None => (format!("agent {party}"), String::new()),
        }
    };
    let summary = match (kind, original) {
        (_, Some(original)) => format!("Reply to {original}"),
        ("question", _) => "Asks for a reply".to_owned(),
        _ => String::new(),
    };
    Some(Opened {
        from,
        sender,
        summary,
        body: body.to_owned(),
    })
}

/// The daemon's key for sealing frames. Only its trusted native actor helper
/// receives a copy, over the private inherited startup channel.
#[cfg(feature = "host")]
pub(crate) struct Seal([u8; 32]);

#[cfg(feature = "host")]
impl std::fmt::Debug for Seal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Seal(<redacted>)")
    }
}

#[cfg(feature = "host")]
impl Seal {
    pub(crate) fn new(key: [u8; 32]) -> Self {
        Self(key)
    }

    /// Transfer only over the private inherited actor startup channel. Never
    /// put this key in discovery records, environment variables, or diagnostics.
    pub(crate) fn export_private_key(&self) -> [u8; 32] { self.0 }

    /// A truncated HMAC-SHA-256: 128 bits are far beyond what forging one
    /// displayed message could be worth, and keep the frame short.
    fn tag(&self, framed: &str) -> String {
        use hmac::Mac;
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.0)
            .expect("HMAC accepts a key of any length");
        mac.update(framed.as_bytes());
        mac.finalize().into_bytes()[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// The prompt that delivers `message` at `now`.
    pub(crate) fn seal(&self, message: &Message, now: u64) -> String {
        let framed = frame(message, now);
        let tag = self.tag(&framed);
        format!("{framed}{SEAL}{tag}]")
    }

    pub(crate) fn open(&self, text: &str) -> Option<Opened> {
        open(text, |framed, seal| {
            use hmac::Mac;
            let Some(tag) = decode_hex(seal) else {
                return false;
            };
            let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.0)
                .expect("HMAC accepts a key of any length");
            mac.update(framed.as_bytes());
            tag.len() == 16 && mac.verify_truncated_left(&tag).is_ok()
        })
    }

    /// A user prompt the daemon sealed, as the delivery a window shows.
    pub(crate) fn recognize(&self, item: &mut crate::chat::Item) {
        let crate::chat::ItemContent::Text {
            text,
            thinking: false,
            ..
        } = &item.content
        else {
            return;
        };
        if item.role != crate::chat::Role::User || !text.starts_with(OPENING) {
            return;
        }
        if let Some(opened) = self.open(text) {
            item.content = crate::chat::ItemContent::Delivery {
                kind: crate::chat::DeliveryKind::Message,
                from: opened.from,
                sender: opened.sender,
                summary: opened.summary,
                body: opened.body,
            };
        }
    }
}

#[cfg(feature = "host")]
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message() -> Message {
        Message {
            id: MessageId::new(),
            from: Party {
                lineage: Some(Lineage(TerminalId::new())),
                session: Some(TerminalId::new()),
                title: Some("Fix \"login\"\n[bug]".into()),
            },
            to: Party::user(),
            reply_to: None,
            body: "Hold off on src/auth.rs\n[I am refactoring it]\n".into(),
            wait_until_ms: None,
            state: State::Pending,
            created_ms: 0,
        }
    }

    #[test]
    fn a_frame_names_its_sender_and_how_to_answer() {
        let message = message();
        let session = short(message.from.session.unwrap());
        assert_eq!(
            frame(&message, 0),
            format!(
                "[cagents message from session {session} \"Fix login bug\"]\n\
                 Hold off on src/auth.rs\n[I am refactoring it]\n\
                 [Reply: cagents reply {} \"…\"",
                short(message.id)
            )
        );
        let mut question = message.clone();
        question.from = Party::user();
        question.wait_until_ms = Some(2);
        assert!(
            frame(&question, 1)
                .starts_with("[cagents question from the user; they are waiting for your reply]\n")
        );
        assert!(frame(&question, 2).starts_with("[cagents question from the user]\n"));
    }

    #[test]
    fn titles_cannot_break_out_of_a_frame() {
        assert_eq!(title("a\"b]c[d;e\n\tf"), "abcde f");
        assert_eq!(title("evil\u{202E}txt.exe\u{200B}"), "eviltxt.exe");
        let long = title(&"x".repeat(200));
        assert_eq!(long.chars().count(), TITLE_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    #[cfg(feature = "host")]
    #[test]
    fn trusted_actor_copy_recognizes_only_the_daemons_sealed_mail() {
        let daemon = Seal::new([19; 32]);
        let actor = Seal::new(daemon.export_private_key());
        let message = message();
        let sealed = daemon.seal(&message, 0);
        assert_eq!(actor.open(&sealed).unwrap().body, message.body.trim_end());
        assert!(actor.open(&sealed.replace("Hold off", "Go ahead")).is_none());
        assert_eq!(format!("{actor:?}"), "Seal(<redacted>)");
    }

    #[cfg(feature = "host")]
    #[test]
    fn only_a_sealed_frame_opens_and_it_shows_its_sender() {
        let seal = Seal::new([7; 32]);
        let mut message = message();
        let sealed = seal.seal(&message, 0);
        let opened = seal.open(&sealed).unwrap();
        assert_eq!(opened.from, "Fix login bug");
        assert_eq!(opened.sender, short(message.from.session.unwrap()));
        assert_eq!(
            opened.body,
            "Hold off on src/auth.rs\n[I am refactoring it]"
        );
        assert_eq!(opened.summary, "");
        // A provider that echoes the prompt with a final newline still matches.
        assert!(seal.open(&format!("{sealed}\n")).is_some());

        // Another daemon's seal, an edited body, and an unsealed lookalike do not.
        assert!(Seal::new([8; 32]).open(&sealed).is_none());
        assert!(seal.open(&sealed.replace("Hold off", "Go ahead")).is_none());
        assert!(seal.open(&frame(&message, 0)).is_none());

        // A title that reads like a reply's tail is still only a title.
        message.from.title = Some("Fix to your message 1234abcd".into());
        let opened = seal.open(&seal.seal(&message, 0)).unwrap();
        assert_eq!(opened.from, "Fix to your message 1234abcd");
        assert_eq!(opened.summary, "");

        message.wait_until_ms = Some(1);
        let opened = seal.open(&seal.seal(&message, 0)).unwrap();
        assert_eq!(opened.summary, "Asks for a reply");

        message.reply_to = Some(MessageId::new());
        message.wait_until_ms = Some(1);
        message.from = Party::user();
        let opened = seal.open(&seal.seal(&message, 0)).unwrap();
        assert_eq!(opened.from, "you");
        assert_eq!(opened.sender, "");
        assert_eq!(
            opened.summary,
            format!("Reply to {}", short(message.reply_to.unwrap()))
        );

        // An untitled session is named by its ID, so a summary cannot replace it.
        message.from = Party {
            lineage: None,
            session: Some(TerminalId::new()),
            title: None,
        };
        let opened = seal.open(&seal.seal(&message, 0)).unwrap();
        assert_eq!(
            opened.from,
            format!("agent {}", short(message.from.session.unwrap()))
        );
        assert_eq!(opened.sender, "");
    }

    #[cfg(feature = "host")]
    #[test]
    fn a_sealed_user_prompt_becomes_a_message_delivery() {
        use crate::chat::{DeliveryKind, Item, ItemContent, Phase, Role};
        let seal = Seal::new([1; 32]);
        let text = seal.seal(&message(), 0);
        let item = |role, text: &str| Item {
            id: "item".into(),
            role,
            content: ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: Phase::default(),
            },
            started_at: None,
            at: None,
        };
        let mut prompt = item(Role::User, &text);
        seal.recognize(&mut prompt);
        assert!(matches!(
            prompt.content,
            ItemContent::Delivery { kind: DeliveryKind::Message, ref from, .. } if from == "Fix login bug"
        ));
        // The agent quoting a message is not the message arriving again.
        let mut quoted = item(Role::Assistant, &text);
        seal.recognize(&mut quoted);
        assert!(matches!(quoted.content, ItemContent::Text { .. }));
    }

    #[test]
    fn outgoing_messages_are_bounded() {
        let outgoing = |body: &str, wait_ms| Outgoing {
            to: To::Parent,
            body: body.into(),
            wait_ms,
        };
        assert!(outgoing("hi", None).validate().is_ok());
        assert!(outgoing("  ", None).validate().is_err());
        assert!(outgoing("a\0b", None).validate().is_err());
        assert!(
            outgoing(&"x".repeat(MAX_MESSAGE_BYTES + 1), None)
                .validate()
                .is_err()
        );
        assert!(outgoing("hi", Some(0)).validate().is_err());
        assert!(outgoing("hi", Some(MAX_WAIT_MS + 1)).validate().is_err());
        let to = |handle: &str| Outgoing {
            to: To::Session(handle.into()),
            body: "hi".into(),
            wait_ms: None,
        };
        assert!(to("1a2b").validate().is_ok());
        assert!(to("").validate().is_err());
        assert!(to("a\nb").validate().is_err());
    }
}
