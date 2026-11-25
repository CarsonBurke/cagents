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
use crate::model::{AgentId, MessageId, RunId, TerminalId};

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

/// A sent message and what its recipient can do with it now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    pub sent: Sent,
    /// The recipient's session as a listing reports it: a usage limit holds
    /// the message until the limit resets, and the sender does not wait on a
    /// reply that cannot come before then. Absent for the user and for a
    /// recipient no session of which runs.
    pub recipient: Option<super::directory::Member>,
}

/// How an agent cagents hosts takes a message: as its next run, so its owner
/// sees the work and its outcome like any prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Handoff {
    /// The message is the prompt of this run of the agent.
    Run {
        agent: AgentId,
        run: RunId,
        start: super::Start,
    },
    /// The message waits until the agent's current work ends, and then
    /// becomes its next run.
    Waiting {
        agent: AgentId,
        behind: Option<RunId>,
    },
    /// The agent cannot take it as a run, for this reason; the message waits
    /// as one to any other session would.
    Held { agent: AgentId, reason: String },
    /// Earlier messages to the agent have not reached it yet; this one waits
    /// behind them, then becomes a run.
    Behind { agent: AgentId },
    /// Every agent slot is taken; the message waits until one frees, then
    /// becomes a run.
    NoSlot { agent: AgentId },
    /// The agent received the message as this run, which then ended without
    /// finishing, for this reason.
    Ended {
        agent: AgentId,
        run: RunId,
        reason: String,
    },
    /// The message is the prompt of this run, which ended, for this reason,
    /// with no proof of whether the agent received it. It is not sent again.
    Unknown {
        agent: AgentId,
        run: RunId,
        reason: String,
    },
}

/// A sent message, what its recipient can do with it now, and, for an agent
/// recipient, how that agent takes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Handed {
    pub delivery: Delivery,
    pub agent: Option<Handoff>,
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
/// What a retained helper advertises when its [`open`] reads every frame
/// [`frame`] writes. Raise the number whenever a frame changes in a way an
/// earlier `open` would not read, as giving the last line its `in a shell`
/// did, and as handing several messages over in one prompt did: a helper
/// outlives the daemon that started it, and the daemon opens on its behalf
/// what a helper without the current number shows.
#[cfg(feature = "host")]
pub(crate) const FRAMES_CAPABILITY: &str = "mailbox-frames-3";
const SEAL: &str = " · seal ";
/// What separates the sealed frames of a prompt that hands over several
/// messages at once.
const BETWEEN_FRAMES: &str = "\n\n";
/// What the last line of a frame starts with. It says where the command
/// runs: `[Reply: cagents reply …]` alone read like a call to a tool named
/// `cagents`, and an agent once made that call.
const REPLY: &str = "[Reply in a shell: ";
/// The last line's start in frames from before it said so, which saved
/// histories keep.
const EARLIER_REPLY: &str = "[Reply: ";

/// The prompt a message becomes at `now`, up to its seal. The first line says
/// who sent it, the last how to answer; the body between them is the sender's
/// own.
pub fn frame(message: &Message, now: u64) -> String {
    // A question that waited out a busy recipient still asks, but no one
    // waits for the answer any more.
    framed(
        message,
        message.wait_until_ms.is_some_and(|until| until > now),
    )
}

/// The prompt `message` becomes, saying the sender waits for the reply only
/// when `waiting`.
fn framed(message: &Message, waiting: bool) -> String {
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
    if waiting {
        heading.push_str("; they are waiting for your reply");
    }
    format!(
        "{heading}]\n{}\n{REPLY}cagents reply {} \"…\"",
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
    parse(framed)
}

/// A frame's words before its seal, read as the message they give.
fn parse(framed: &str) -> Option<Opened> {
    let (heading, rest) = framed.strip_prefix(OPENING)?.split_once("]\n")?;
    // The later of the two is the frame's own last line; the other, if
    // present at all, is the sender's words.
    let (body, _) = [REPLY, EARLIER_REPLY]
        .into_iter()
        .filter_map(|reply| rest.rsplit_once(&*format!("\n{reply}")))
        .max_by_key(|(body, _)| body.len())?;
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

/// Reads `text` as one or more sealed frames, one after another, each of
/// which `seals` accepts, as [`Seal::seal_all`] writes them. Anything else,
/// including a frame followed by words of a person's own, is no delivery.
///
/// Each frame ends at the first seal after its start that `seals` accepts: a
/// seal is a MAC over the frame before it, so neither a seal quoted in a body
/// nor a whole frame copied into one ends the frame that carries it. The MAC
/// is carried forward from one possible end to the next, so a body full of
/// seal-like words costs one pass over the text, not one per word.
pub fn open_all<S: Seals>(text: &str, seals: &S) -> Option<Vec<Opened>> {
    let mut rest = text.strip_suffix('\n').unwrap_or(text);
    let mut opened = Vec::new();
    loop {
        if !rest.starts_with(OPENING) {
            return None;
        }
        let mut running = seals.start();
        let mut read = 0;
        let mut end = None;
        for (at, _) in rest.match_indices(SEAL) {
            let tag = at + SEAL.len();
            if rest.len() < tag + SEAL_DIGITS {
                break;
            }
            // Not on a character boundary, so no seal.
            let Some(seal) = rest.get(tag..tag + SEAL_DIGITS) else {
                continue;
            };
            let Some(after) = rest[tag + SEAL_DIGITS..].strip_prefix(']') else {
                continue;
            };
            let next = match after.strip_prefix(BETWEEN_FRAMES) {
                _ if after.is_empty() => None,
                Some(next) if next.starts_with(OPENING) => Some(next),
                _ => continue,
            };
            if !seal.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            seals.extend(&mut running, &rest[read..at]);
            read = at;
            // A seal judged only by its shape may end a frame that is not one.
            if seals.accepts(running.clone(), seal)
                && let Some(message) = parse(&rest[..at])
            {
                end = Some((message, next));
                break;
            }
        }
        let (message, next) = end?;
        opened.push(message);
        match next {
            Some(next) => rest = next,
            None => return Some(opened),
        }
    }
}

/// How many hexadecimal digits a frame's seal has.
const SEAL_DIGITS: usize = 32;

/// Judges the seals of the frames [`open_all`] reads, over each frame's
/// words as it reads them, from the frame's start.
pub trait Seals {
    /// What has been read of a frame so far.
    type Running: Clone;
    fn start(&self) -> Self::Running;
    fn extend(&self, running: &mut Self::Running, text: &str);
    /// Whether `seal` seals what was read.
    fn accepts(&self, running: Self::Running, seal: &str) -> bool;
}

/// Accepts any seal of the right shape. It proves nothing, so a window may
/// use it only to recognize what looks like the courier's prompt.
pub struct SealShaped;

impl Seals for SealShaped {
    type Running = ();
    fn start(&self) {}
    fn extend(&self, _: &mut (), _: &str) {}
    fn accepts(&self, (): (), _: &str) -> bool {
        true
    }
}

/// What a window shows for the frames of one prompt: a message as it was
/// sent, or several as one delivery named by their senders, whose body gives
/// each message under its sender's name.
#[cfg(feature = "host")]
fn delivery(mut opened: Vec<Opened>) -> crate::chat::ItemContent {
    use crate::chat::{DeliveryKind, ItemContent};
    if opened.len() == 1 {
        let opened = opened.remove(0);
        return ItemContent::Delivery {
            kind: DeliveryKind::Message,
            from: opened.from,
            sender: opened.sender,
            summary: opened.summary,
            body: opened.body,
        };
    }
    let mut senders: Vec<&str> = Vec::new();
    for frame in &opened {
        if !senders.contains(&frame.from.as_str()) {
            senders.push(&frame.from);
        }
    }
    let named = match senders.as_slice() {
        [one] => (*one).to_owned(),
        [first, second] => format!("{first} and {second}"),
        [first, second, third] => format!("{first}, {second} and {third}"),
        [first, second, rest @ ..] => format!("{first}, {second} and {} others", rest.len()),
        [] => unreachable!("a delivery holds at least one frame"),
    };
    let body = opened
        .iter()
        .map(|frame| {
            let mut heading = format!("**{}**", markdown_literal(&frame.from));
            if !frame.sender.is_empty() {
                heading.push_str(&format!(" ({})", frame.sender));
            }
            if !frame.summary.is_empty() {
                heading.push_str(&format!(" · {}", frame.summary));
            }
            // A code block one message leaves open would swallow the rest.
            let body = frame.body.trim_end();
            match open_fence(body) {
                Some(fence) => format!("{heading}\n\n{body}\n{fence}"),
                None => format!("{heading}\n\n{body}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    ItemContent::Delivery {
        kind: DeliveryKind::Message,
        from: String::new(),
        sender: String::new(),
        summary: format!("{} messages from {named}", opened.len()),
        body,
    }
}

/// The fence that closes the code block `markdown` leaves open, if it does,
/// as CommonMark reads fences.
#[cfg(feature = "host")]
fn open_fence(markdown: &str) -> Option<String> {
    let mut open: Option<(char, usize)> = None;
    for line in markdown.lines() {
        let indented = line.trim_start_matches(' ');
        if line.len() - indented.len() > 3 {
            continue;
        }
        let line = indented;
        let Some(mark @ ('`' | '~')) = line.chars().next() else {
            continue;
        };
        let length = line.chars().take_while(|&c| c == mark).count();
        if length < 3 {
            continue;
        }
        let info = &line[length..];
        open = match open {
            None if mark == '`' && info.contains('`') => None,
            None => Some((mark, length)),
            Some((opened, at_least))
                if mark == opened && length >= at_least && info.trim().is_empty() =>
            {
                None
            }
            still => still,
        };
    }
    open.map(|(mark, length)| mark.to_string().repeat(length))
}

/// `text` escaped so that Markdown shows it as written.
#[cfg(feature = "host")]
fn markdown_literal(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii_punctuation() {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// The daemon-side signer for courier frames. Retained actors receive only a
/// recognizer API, through authenticated private bootstrap. That recognizer
/// still holds the registry-wide symmetric MAC material: matched helper code is
/// inside the daemon's trusted realm, and this is not compromise containment.
#[cfg(feature = "host")]
pub struct Seal([u8; 32]);

#[cfg(feature = "host")]
impl std::fmt::Debug for Seal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Seal(<redacted>)")
    }
}

#[cfg(feature = "host")]
impl Seal {
    pub fn new(key: [u8; 32]) -> Self {
        Self(key)
    }

    pub fn recognizer(&self) -> Recognizer {
        Recognizer(self.0)
    }

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
    pub fn seal(&self, message: &Message, now: u64) -> String {
        self.sealed(frame(message, now))
    }

    /// The one prompt that delivers `messages` at `now`, in order: each
    /// message's own sealed frame, so that each still says who sent it and
    /// how to answer it, and a window shows them as messages only when every
    /// frame is the daemon's.
    pub fn seal_all<'a>(
        &self,
        messages: impl IntoIterator<Item = &'a Message>,
        now: u64,
    ) -> String {
        messages
            .into_iter()
            .map(|message| self.seal(message, now))
            .collect::<Vec<_>>()
            .join(BETWEEN_FRAMES)
    }

    /// The prompt of the agent run that delivers `message`. A run can start
    /// long after it is accepted, so it never says the sender is waiting.
    pub fn seal_run(&self, message: &Message) -> String {
        self.sealed(framed(message, false))
    }

    fn sealed(&self, framed: String) -> String {
        let tag = self.tag(&framed);
        format!("{framed}{SEAL}{tag}]")
    }
}

/// Recognition authority used by a Store; it exposes no frame-signing method.
/// HMAC verification remains symmetric, so a compromised holder of this value
/// could recover the shared signing material. Providers never receive it.
#[cfg(feature = "host")]
#[derive(Clone)]
pub struct Recognizer([u8; 32]);

#[cfg(feature = "host")]
impl std::fmt::Debug for Recognizer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Recognizer(<redacted>)")
    }
}

/// Serialized only on the inherited, authenticated retained-owner startup
/// channel. Never store in discovery, process environment, journals or logs.
#[cfg(feature = "host")]
#[derive(Serialize, Deserialize)]
pub(crate) struct RetainedRecognition([u8; 32]);

#[cfg(feature = "host")]
impl std::fmt::Debug for RetainedRecognition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RetainedRecognition(<redacted>)")
    }
}

#[cfg(feature = "host")]
impl RetainedRecognition {
    pub(crate) fn into_recognizer(self) -> Recognizer {
        Recognizer(self.0)
    }
}

#[cfg(feature = "host")]
impl Seals for Recognizer {
    type Running = hmac::Hmac<sha2::Sha256>;

    fn start(&self) -> Self::Running {
        use hmac::Mac;
        hmac::Hmac::new_from_slice(&self.0).expect("HMAC accepts a key of any length")
    }

    fn extend(&self, running: &mut Self::Running, text: &str) {
        use hmac::Mac;
        running.update(text.as_bytes());
    }

    fn accepts(&self, running: Self::Running, seal: &str) -> bool {
        use hmac::Mac;
        decode_hex(seal)
            .is_some_and(|tag| tag.len() == 16 && running.verify_truncated_left(&tag).is_ok())
    }
}

#[cfg(feature = "host")]
impl Recognizer {
    /// Explicitly delegate recognition to the trusted retained actor, never to
    /// its provider. This is a typed API restriction, not asymmetric crypto.
    pub(crate) fn delegate_to_retained_owner(&self) -> RetainedRecognition {
        RetainedRecognition(self.0)
    }

    #[cfg(test)]
    pub(crate) fn open(&self, text: &str) -> Option<Opened> {
        open(text, |framed, seal| {
            let mut running = self.start();
            self.extend(&mut running, framed);
            self.accepts(running, seal)
        })
    }

    /// As [`open_all`], for the frames this daemon sealed.
    pub(crate) fn open_all(&self, text: &str) -> Option<Vec<Opened>> {
        open_all(text, self)
    }

    /// A user prompt the daemon sealed, as the delivery a window shows.
    /// Whether it was one.
    pub fn recognize(&self, item: &mut crate::chat::Item) -> bool {
        if !may_be_frame(item) {
            return false;
        }
        let crate::chat::ItemContent::Text { text, .. } = &item.content else {
            return false;
        };
        let Some(opened) = self.open_all(text) else {
            return false;
        };
        item.content = delivery(opened);
        true
    }

    /// Opens the frames among `snapshot`'s items, copying only those it
    /// opens. An opened message keeps no copy source, as one a current
    /// helper opens has none: its source would copy the frame back.
    pub(crate) fn recognize_snapshot(&self, snapshot: &mut crate::chat::Snapshot) {
        for (index, item) in snapshot.items.iter_mut().enumerate() {
            if !may_be_frame(item) {
                continue;
            }
            let mut opened = crate::chat::Item::clone(item);
            if self.recognize(&mut opened) {
                *item = std::sync::Arc::new(opened);
                if let Some(source) = snapshot.text_copy_sources.conversation.get_mut(index) {
                    *source = None;
                }
            }
        }
    }

    /// As [`Self::recognize_snapshot`], for a transcript read or its update
    /// on its way to a window. One still encoded is decoded only if its bytes hold a frame's
    /// opening at all, and passes as it came if it does not decode.
    pub(crate) fn recognize_presentation(
        &self,
        response: crate::terminal_ipc::PreparedResponse,
    ) -> crate::terminal_ipc::PreparedResponse {
        use crate::terminal_ipc::{PreparedResponse, ServerResponse};
        let mut response = match response {
            PreparedResponse::EncodedChat { chat_id, bytes } => {
                if !bytes
                    .windows(OPENING.len())
                    .any(|window| window == OPENING.as_bytes())
                {
                    return PreparedResponse::EncodedChat { chat_id, bytes };
                }
                match postcard::from_bytes::<ServerResponse>(&bytes) {
                    Ok(response) => response,
                    Err(error) => {
                        tracing::warn!(%error, "a retained transcript read did not decode; its frames stay closed");
                        return PreparedResponse::EncodedChat { chat_id, bytes };
                    }
                }
            }
            PreparedResponse::Decoded(response) => response,
        };
        match &mut response {
            ServerResponse::ChatSnapshot {
                snapshot: Some(snapshot),
                ..
            } => self.recognize_snapshot(snapshot),
            ServerResponse::ChatUpdate { update, .. } => {
                for change in &mut update.changes {
                    // A prompt arrives whole; only a reply streams.
                    if let crate::chat::presentation::ItemChange::Replace(item) = change {
                        self.recognize(item);
                    }
                }
            }
            _ => {}
        }
        PreparedResponse::Decoded(response)
    }
}

/// Whether `item` is shaped as a frame the daemon seals: a user prompt that
/// opens as one. Only a seal makes it one.
#[cfg(feature = "host")]
fn may_be_frame(item: &crate::chat::Item) -> bool {
    item.role == crate::chat::Role::User
        && matches!(&item.content, crate::chat::ItemContent::Text { text, thinking: false, .. }
            if text.starts_with(OPENING))
}

#[cfg(feature = "host")]
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
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

    /// A frame changed so that an earlier [`open`] would not read it must
    /// come with a new `FRAMES_CAPABILITY`, or a running helper built before
    /// the change shows the frame as a person's prompt.
    #[test]
    fn a_frame_names_its_sender_and_how_to_answer() {
        #[cfg(feature = "host")]
        assert_eq!(FRAMES_CAPABILITY, "mailbox-frames-3");
        let message = message();
        let session = short(message.from.session.unwrap());
        assert_eq!(
            frame(&message, 0),
            format!(
                "[cagents message from session {session} \"Fix login bug\"]\n\
                 Hold off on src/auth.rs\n[I am refactoring it]\n\
                 [Reply in a shell: cagents reply {} \"…\"",
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
    fn only_a_sealed_frame_opens_and_it_shows_its_sender() {
        let seal = Seal::new([7; 32]);
        let mut message = message();
        let sealed = seal.seal(&message, 0);
        let opened = seal.recognizer().open(&sealed).unwrap();
        assert_eq!(opened.from, "Fix login bug");
        assert_eq!(opened.sender, short(message.from.session.unwrap()));
        assert_eq!(
            opened.body,
            "Hold off on src/auth.rs\n[I am refactoring it]"
        );
        assert_eq!(opened.summary, "");
        // A provider that echoes the prompt with a final newline still matches.
        assert!(seal.recognizer().open(&format!("{sealed}\n")).is_some());

        // Another daemon's seal, an edited body, and an unsealed lookalike do not.
        assert!(Seal::new([8; 32]).recognizer().open(&sealed).is_none());
        assert!(
            seal.recognizer()
                .open(&sealed.replace("Hold off", "Go ahead"))
                .is_none()
        );
        assert!(seal.recognizer().open(&frame(&message, 0)).is_none());

        // A frame from before the last line said where to run it, as saved
        // histories hold, still opens; so does a body that quotes the new
        // last line.
        let earlier = frame(&message, 0).replace(REPLY, EARLIER_REPLY);
        let earlier = format!("{earlier}{SEAL}{}]", seal.tag(&earlier));
        assert_eq!(
            seal.recognizer().open(&earlier).unwrap().body,
            "Hold off on src/auth.rs\n[I am refactoring it]"
        );
        for trailer in [REPLY, EARLIER_REPLY] {
            let mut quoting = message.clone();
            quoting.body =
                format!("See this:\n{trailer}cagents reply 1234abcd \"…\"]\nthen answer");
            assert_eq!(
                seal.recognizer()
                    .open(&seal.seal(&quoting, 0))
                    .unwrap()
                    .body,
                quoting.body
            );
        }

        // A question taken as an agent run is still a question, but the run
        // may start after the asker stopped waiting, so it never says they
        // wait; it opens like any sealed message.
        let mut question = message.clone();
        question.wait_until_ms = Some(u64::MAX);
        let run = seal.seal_run(&question);
        assert!(run.starts_with("[cagents question from session"), "{run}");
        assert!(!run.contains("waiting for your reply"), "{run}");
        assert_eq!(
            run,
            seal.seal_run(&question),
            "a run's prompt is its retry key's"
        );
        assert!(seal.recognizer().open(&run).is_some());

        // A title that reads like a reply's tail is still only a title.
        message.from.title = Some("Fix to your message 1234abcd".into());
        let opened = seal.recognizer().open(&seal.seal(&message, 0)).unwrap();
        assert_eq!(opened.from, "Fix to your message 1234abcd");
        assert_eq!(opened.summary, "");

        message.wait_until_ms = Some(1);
        let opened = seal.recognizer().open(&seal.seal(&message, 0)).unwrap();
        assert_eq!(opened.summary, "Asks for a reply");

        message.reply_to = Some(MessageId::new());
        message.wait_until_ms = Some(1);
        message.from = Party::user();
        let opened = seal.recognizer().open(&seal.seal(&message, 0)).unwrap();
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
        let opened = seal.recognizer().open(&seal.seal(&message, 0)).unwrap();
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
        seal.recognizer().recognize(&mut prompt);
        assert!(matches!(
            prompt.content,
            ItemContent::Delivery { kind: DeliveryKind::Message, ref from, .. } if from == "Fix login bug"
        ));
        // The agent quoting a message is not the message arriving again.
        let mut quoted = item(Role::Assistant, &text);
        seal.recognizer().recognize(&mut quoted);
        assert!(matches!(quoted.content, ItemContent::Text { .. }));
    }

    #[cfg(feature = "host")]
    #[test]
    fn messages_handed_over_together_open_in_order_and_show_as_one_delivery() {
        use crate::chat::{DeliveryKind, Item, ItemContent, Phase, Role};
        let seal = Seal::new([5; 32]);
        let recognizer = seal.recognizer();
        let first = message();
        let mut second = message();
        second.from.title = Some("Review *auth*".into());
        // A body that quotes a seal, or a whole frame, does not end its own.
        second.body = format!(
            "Seen this?\n\n{}\n\nand this · seal {}]\n\nSo far so good.",
            seal.seal(&first, 0),
            "0".repeat(32)
        );
        let mut third = message();
        third.from = Party::user();
        third.reply_to = Some(MessageId::new());
        let sealed = seal.seal_all([&first, &second, &third], 0);
        assert!(sealed.starts_with(&seal.seal(&first, 0)));
        assert_eq!(
            seal.seal_all([&first], 0),
            seal.seal(&first, 0),
            "one message is handed over as it always was"
        );

        let opened = recognizer.open_all(&sealed).unwrap();
        assert_eq!(
            opened
                .iter()
                .map(|opened| opened.body.as_str())
                .collect::<Vec<_>>(),
            [
                "Hold off on src/auth.rs\n[I am refactoring it]",
                second.body.as_str(),
                "Hold off on src/auth.rs\n[I am refactoring it]",
            ]
        );
        assert_eq!(opened[1].from, "Review *auth*");
        assert!(recognizer.open_all(&format!("{sealed}\n")).is_some());
        assert!(
            recognizer.open(&sealed).is_none(),
            "an earlier helper's open reads none of it"
        );

        // Every frame must be the daemon's, with nothing else around them.
        let forged = seal.seal_all([&first], 0) + "\n\n" + &frame(&third, 0);
        assert!(recognizer.open_all(&forged).is_none());
        let edited = sealed.replacen("So far so good", "Go ahead", 1);
        assert!(recognizer.open_all(&edited).is_none());
        assert!(
            recognizer
                .open_all(&format!("{sealed}\n\nand my own words"))
                .is_none()
        );
        assert!(recognizer.open_all(&format!("Look:\n\n{sealed}")).is_none());
        assert!(Seal::new([6; 32]).recognizer().open_all(&sealed).is_none());

        let mut item = Item {
            id: "item".into(),
            role: Role::User,
            content: ItemContent::Text {
                text: sealed,
                thinking: false,
                phase: Phase::default(),
            },
            started_at: None,
            at: None,
        };
        assert!(recognizer.recognize(&mut item));
        let ItemContent::Delivery {
            kind: DeliveryKind::Message,
            from,
            sender,
            summary,
            body,
        } = &item.content
        else {
            panic!("{:?}", item.content);
        };
        assert_eq!((from.as_str(), sender.as_str()), ("", ""));
        assert_eq!(
            summary,
            "3 messages from Fix login bug, Review *auth* and you"
        );
        let sections: Vec<&str> = body.split("\n\n---\n\n").collect();
        assert_eq!(sections.len(), 3, "{body}");
        let session = short(first.from.session.unwrap());
        assert!(
            sections[0].starts_with(&format!("**Fix login bug** ({session})\n\n")),
            "{body}"
        );
        // A title shows as written, not as Markdown.
        assert!(sections[1].starts_with("**Review \\*auth\\*** ("), "{body}");
        assert!(sections[1].ends_with("So far so good."), "{body}");
        assert!(
            sections[2].starts_with(&format!(
                "**you** · Reply to {}\n\n",
                short(third.reply_to.unwrap())
            )),
            "{body}"
        );
    }
    #[cfg(feature = "host")]
    #[test]
    fn a_body_full_of_frame_boundaries_ends_only_where_its_seal_does() {
        let seal = Seal::new([7; 32]);
        let recognizer = seal.recognizer();
        // Each of these looks like the end of one frame and the start of the
        // next, so every one of them is a seal to check.
        let boundary = format!(
            "{SEAL}{}]{BETWEEN_FRAMES}{OPENING}",
            "a".repeat(SEAL_DIGITS)
        );
        let mut message = message();
        message.body = boundary.repeat(MAX_MESSAGE_BYTES / boundary.len() - 1);
        // Nor does one whose digits would end inside a character.
        message
            .body
            .push_str(&format!("{SEAL}{}🎉]", "a".repeat(SEAL_DIGITS - 1)));
        assert!(message.body.len() <= MAX_MESSAGE_BYTES);
        let messages = vec![message; 8];
        let sealed = seal.seal_all(messages.iter(), 0);

        let opened = recognizer.open_all(&sealed).expect("the daemon's frames");
        assert_eq!(opened.len(), messages.len());
        assert!(opened.iter().all(|opened| opened.body == messages[0].body));
        // A window that reads seals only by their shape still sees a message.
        let one = open_all(&seal.seal(&messages[0], 0), &SealShaped).expect("frame-shaped");
        assert_eq!(one.len(), 1);
    }

    #[cfg(feature = "host")]
    #[test]
    fn a_code_block_one_message_leaves_open_ends_with_it() {
        use crate::chat::ItemContent;
        assert_eq!(open_fence("```rust\nfn main() {"), Some("```".into()));
        assert_eq!(open_fence("~~~~\n~~~\nstill code"), Some("~~~~".into()));
        assert_eq!(open_fence("  ```\ncode\n```"), None);
        assert_eq!(open_fence("```sh\n``` not a close\n````"), None);
        assert_eq!(open_fence("``` not `a` fence"), None);
        assert_eq!(open_fence("    ```\nindented code"), None);

        let mut first = message();
        first.body = "Here is the failing part:\n```rust\nfn main() {".into();
        let mut second = message();
        second.body = "# Landed\n\nThe fix is on main.".into();
        let seal = Seal::new([8; 32]);
        let opened = seal
            .recognizer()
            .open_all(&seal.seal_all([&first, &second], 0))
            .unwrap();
        let ItemContent::Delivery { body, .. } = delivery(opened) else {
            panic!("a delivery");
        };
        let sections: Vec<&str> = body.split("\n\n---\n\n").collect();
        assert!(sections[0].ends_with("fn main() {\n```"), "{body}");
        assert!(
            sections[1].ends_with("# Landed\n\nThe fix is on main."),
            "{body}"
        );
    }

    #[cfg(feature = "host")]
    fn prompt(id: &str, role: crate::chat::Role, text: &str) -> std::sync::Arc<crate::chat::Item> {
        std::sync::Arc::new(crate::chat::Item {
            id: id.into(),
            role,
            content: crate::chat::ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: crate::chat::Phase::default(),
            },
            started_at: None,
            at: None,
        })
    }

    /// The window a helper built before the current frames shows: every
    /// frame still a person's prompt.
    #[cfg(feature = "host")]
    fn older_helper_window(seal: &Seal, sent: &Message) -> crate::chat::Snapshot {
        use crate::chat::Role;
        let mut reply = message();
        reply.reply_to = Some(MessageId::new());
        let mut question = message();
        question.from = Party::user();
        question.wait_until_ms = Some(9);
        let forged = frame(&message(), 0) + " · seal 00000000000000000000000000000000]";
        // A frame as a helper before `in a shell` was written, still opened.
        let earlier = seal.sealed(frame(&message(), 0).replacen(REPLY, EARLIER_REPLY, 1));
        let items = vec![
            prompt("message", Role::User, &seal.seal(sent, 0)),
            prompt("reply", Role::User, &seal.seal_run(&reply)),
            prompt("question", Role::User, &seal.seal(&question, 1)),
            prompt("forged", Role::User, &forged),
            prompt("person", Role::User, "[cagents is great] thanks"),
            prompt("quoted", Role::Assistant, &seal.seal(&message(), 0)),
            prompt("earlier", Role::User, &earlier),
        ];
        let mut window = crate::chat::Snapshot::default();
        window.text_copy_sources.conversation = items.iter().map(|_| Some(copy_source())).collect();
        window.items = items;
        window
    }

    #[cfg(feature = "host")]
    fn copy_source() -> crate::chat::TextCopySource {
        crate::chat::TextCopySource {
            cache: uuid::Uuid::nil(),
            scope: crate::chat::TextCopyScope::Conversation,
            token: 1,
            revision: 1,
            bytes: 1,
        }
    }

    #[cfg(feature = "host")]
    fn kinds(snapshot: &crate::chat::Snapshot) -> Vec<(String, String)> {
        use crate::chat::ItemContent;
        snapshot
            .items
            .iter()
            .map(|item| {
                let shown = match &item.content {
                    ItemContent::Delivery {
                        from,
                        sender,
                        summary,
                        body,
                        ..
                    } => {
                        assert!(!body.contains("[Reply in a shell") && !body.contains(" · seal "));
                        format!("message from {from} ({sender}) {summary}")
                    }
                    _ => "text".into(),
                };
                (item.id.clone(), shown.trim_end().into())
            })
            .collect()
    }

    #[cfg(feature = "host")]
    #[test]
    fn the_daemon_opens_every_sealed_kind_a_window_shows_and_nothing_else() {
        let seal = Seal::new([3; 32]);
        let sent = message();
        let mut window = older_helper_window(&seal, &sent);
        let untouched = [window.items[3].clone(), window.items[4].clone()];
        seal.recognizer().recognize_snapshot(&mut window);
        let shown = kinds(&window);
        let sender = sent.from.short().unwrap();
        assert_eq!(shown[0].1, format!("message from Fix login bug ({sender})"));
        assert!(
            shown[1].1.starts_with("message from Fix login bug (")
                && shown[1].1.contains("Reply to "),
            "{shown:?}"
        );
        assert_eq!(shown[2].1, "message from you () Asks for a reply");
        // Neither a frame without a valid seal, a person's own words, nor an
        // agent quoting a message is a message arriving.
        assert_eq!(
            shown[3..6],
            [
                ("forged".to_owned(), "text".to_owned()),
                ("person".to_owned(), "text".to_owned()),
                ("quoted".to_owned(), "text".to_owned()),
            ]
        );
        assert!(
            shown[6].1.starts_with("message from Fix login bug ("),
            "{shown:?}"
        );
        // What is not opened is not copied, and keeps its copy source; an
        // opened message would copy its frame back, so it keeps none.
        assert!(std::sync::Arc::ptr_eq(&window.items[3], &untouched[0]));
        assert!(std::sync::Arc::ptr_eq(&window.items[4], &untouched[1]));
        let copyable: Vec<bool> = window
            .text_copy_sources
            .conversation
            .iter()
            .map(Option::is_some)
            .collect();
        assert_eq!(copyable, [false, false, false, true, true, true, false]);
    }

    #[cfg(feature = "host")]
    #[test]
    fn a_retained_read_is_opened_whether_it_arrives_decoded_encoded_or_as_an_update() {
        use crate::chat::presentation::{ItemChange, Presentation};
        use crate::terminal_ipc::{PreparedResponse, ServerResponse};
        let seal = Seal::new([4; 32]);
        let recognizer = seal.recognizer();
        let chat_id = TerminalId::new();
        let read = |snapshot: crate::chat::Snapshot| ServerResponse::ChatSnapshot {
            chat_id,
            snapshot: Some(snapshot),
        };
        let opened = |response: PreparedResponse| match response.decode().unwrap() {
            ServerResponse::ChatSnapshot {
                snapshot: Some(snapshot),
                ..
            } => kinds(&snapshot),
            other => panic!("{other:?}"),
        };
        // Every frame names its message by a fresh ID, so all three reads
        // carry this one window.
        let window = older_helper_window(&seal, &message());
        let expected = {
            let mut window = window.clone();
            recognizer.recognize_snapshot(&mut window);
            kinds(&window)
        };
        // A helper speaking an older wire answers decoded.
        let decoded =
            recognizer.recognize_presentation(PreparedResponse::Decoded(read(window.clone())));
        assert_eq!(opened(decoded), expected);
        // One speaking this wire answers in bytes.
        let bytes = postcard::to_allocvec(&read(window)).unwrap();
        let encoded =
            recognizer.recognize_presentation(PreparedResponse::EncodedChat { chat_id, bytes });
        assert_eq!(opened(encoded), expected);
        // Bytes holding no frame are relayed as they came, undecoded.
        let plain = crate::chat::Snapshot {
            items: vec![prompt("person", crate::chat::Role::User, "Fix the build")],
            ..Default::default()
        };
        let bytes = postcard::to_allocvec(&read(plain)).unwrap();
        match recognizer.recognize_presentation(PreparedResponse::EncodedChat {
            chat_id,
            bytes: bytes.clone(),
        }) {
            PreparedResponse::EncodedChat { bytes: relayed, .. } => assert_eq!(relayed, bytes),
            PreparedResponse::Decoded(_) => panic!("a read without a frame was decoded"),
        }
        // Bytes that do not decode are relayed too, rather than failing the read.
        let garbled = b"\xff[cagents message".to_vec();
        assert!(matches!(
            recognizer.recognize_presentation(PreparedResponse::EncodedChat {
                chat_id,
                bytes: garbled
            }),
            PreparedResponse::EncodedChat { .. }
        ));
        // A prompt that arrives after the window is in an update.
        let update = ServerResponse::ChatUpdate {
            chat_id,
            update: Presentation {
                base_generation: 1,
                file_changes_unchanged: true,
                metadata: Default::default(),
                order: vec!["message".into()],
                changes: vec![ItemChange::Replace(crate::chat::Item::clone(&prompt(
                    "message",
                    crate::chat::Role::User,
                    &seal.seal(&message(), 0),
                )))],
                children: Vec::new(),
            },
        };
        match recognizer
            .recognize_presentation(PreparedResponse::Decoded(update))
            .decode()
            .unwrap()
        {
            ServerResponse::ChatUpdate { update, .. } => assert!(matches!(
                &update.changes[0],
                ItemChange::Replace(crate::chat::Item {
                    content: crate::chat::ItemContent::Delivery { from, .. },
                    ..
                }) if from == "Fix login bug"
            )),
            other => panic!("{other:?}"),
        }
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
