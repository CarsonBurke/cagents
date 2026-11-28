//! Messages between sessions.
//!
//! A session that needs another's attention, or the user at their own CLI,
//! leaves a message for that session's lineage, so it still arrives after the
//! session is relaunched or recovered. The daemon hands a message to a live
//! conversation as a prompt once it is idle; a session that runs in a terminal
//! reads its messages with `cagents inbox`.
//!
//! A delivered message is ordinary prompt text, framed so that the agent knows
//! who sent it and how to answer. The daemon records delivery evidence apart
//! from the prompt, so a window can distinguish mail from a lookalike.

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
/// Legacy wire capability older daemons use to decide who verifies seals.
#[cfg(feature = "host")]
pub(crate) const FRAMES_CAPABILITY: &str = "mailbox-frames-3";
/// Retained helpers with this capability accept evidence alongside input.
#[cfg(feature = "host")]
pub(crate) const EVIDENCE_CAPABILITY: &str = "mailbox-digests-1";
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

/// The prompt a message becomes at `now`, before the final bracket. The first line says
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

/// A complete prompt and the boundaries of the frames the courier wrote.
/// Boundaries are kept alongside the digest because a sender can quote a
/// whole frame in its own message.
#[cfg(feature = "host")]
pub struct FramedPrompt {
    pub text: String,
    lengths: Vec<u32>,
}

#[cfg(feature = "host")]
#[derive(Serialize, Deserialize)]
pub struct FrameEvidence {
    pub digest: [u8; 32],
    pub lengths: Vec<u32>,
}

#[cfg(feature = "host")]
fn digest(text: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes()).into()
}

#[cfg(feature = "host")]
impl FramedPrompt {
    pub fn batch<'a>(messages: impl IntoIterator<Item = &'a Message>, now: u64) -> Self {
        let mut text = String::new();
        let mut lengths = Vec::new();
        for message in messages {
            if !lengths.is_empty() {
                text.push_str(BETWEEN_FRAMES);
            }
            let start = text.len();
            text.push_str(&frame(message, now));
            text.push(']');
            lengths.push((text.len() - start) as u32);
        }
        Self { text, lengths }
    }

    pub fn evidence(&self, seal: &Seal, recipient: TerminalId) -> FrameEvidence {
        FrameEvidence {
            digest: seal.delivery_key(recipient, &digest(&self.text)),
            lengths: self.lengths.clone(),
        }
    }
}

#[cfg(feature = "host")]
fn open_recorded(mut text: &str, lengths: &[u32]) -> Option<Vec<Opened>> {
    if !(1..=MAX_PENDING).contains(&lengths.len()) {
        return None;
    }
    let mut opened = Vec::with_capacity(lengths.len());
    for (index, length) in lengths.iter().enumerate() {
        if index != 0 {
            text = text.strip_prefix(BETWEEN_FRAMES)?;
        }
        let (frame, rest) = text.split_at_checked(*length as usize)?;
        opened.push(parse(frame.strip_suffix(']')?)?);
        text = rest;
    }
    text.is_empty().then_some(opened)
}

/// The title heuristic only needs the shape of a frame. Presentation always
/// requires authenticated evidence.
#[cfg(feature = "host")]
pub(crate) fn looks_like_delivery(text: &str) -> bool {
    looks_like_recorded(text) || open_all(text, &SealShaped).is_some()
}

#[cfg(feature = "host")]
fn looks_like_recorded(text: &str) -> bool {
    text.starts_with(OPENING)
        && text.lines().next_back().is_some_and(|line| {
            let Some(reply) = line.strip_prefix(REPLY) else {
                return false;
            };
            let Some(id) = reply
                .strip_prefix("cagents reply ")
                .and_then(|tail| tail.strip_suffix(" \"…\"]"))
            else {
                return false;
            };
            id.len() == 8 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
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

/// What a question says of itself where a window lists it.
const ASKS: &str = "Asks for a reply";

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
        ("question", _) => ASKS.to_owned(),
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

#[cfg(feature = "host")]
const BATCH_SEPARATOR: &str = "\n\n---\n\n";

/// What a window shows for the frames of one prompt: a message as it was
/// sent, or several as one delivery named by their senders, whose body gives
/// each message under its sender's name.
#[cfg(feature = "host")]
fn delivery(mut opened: Vec<Opened>) -> crate::chat::ItemContent {
    use crate::chat::{DeliveryKind, ItemContent};
    use std::fmt::Write as _;
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
    let mut body = String::new();
    for frame in &opened {
        let start = body.len();
        write!(body, "**{}**", markdown_literal(&frame.from)).unwrap();
        if !frame.sender.is_empty() {
            write!(body, " ({})", frame.sender).unwrap();
        }
        if !frame.summary.is_empty() {
            write!(body, " · {}", frame.summary).unwrap();
        }
        body.push_str("\n\n");
        body.push_str(frame.body.trim_end());
        let end = body.len();
        body.push_str(BATCH_SEPARATOR);
        if let Some(fence) = open_fence(&body[start..]) {
            let closer = format!("\n{fence}");
            body.insert_str(end, &closer);
        }
    }
    body.truncate(body.len() - BATCH_SEPARATOR.len());
    ItemContent::Delivery {
        kind: DeliveryKind::Message,
        from: String::new(),
        sender: String::new(),
        // A sender waiting on an answer is worth seeing without opening it.
        summary: match opened.iter().filter(|frame| frame.summary == ASKS).count() {
            0 => format!("{} messages from {named}", opened.len()),
            1 => format!(
                "{} messages from {named} · 1 asks for a reply",
                opened.len()
            ),
            asking => format!(
                "{} messages from {named} · {asking} ask for a reply",
                opened.len()
            ),
        },
        body,
    }
}

/// Find the fence that consumes the batch separator. An unindented rule
/// already ends a fence inside a list item or block quote.
#[cfg(feature = "host")]
fn open_fence(section: &str) -> Option<&str> {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};

    if !section.contains(['`', '~']) {
        return None;
    }
    let rule = section.len() - BATCH_SEPARATOR.trim_start().len();
    let probe = without_html_blocks(section);
    Parser::new_ext(&probe, Options::ENABLE_TABLES)
        .into_offset_iter()
        .find_map(|(event, range)| {
            if range.start < rule
                && rule < range.end
                && matches!(
                    event,
                    Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_)))
                )
            {
                let opening = &section[range.start..];
                let mark = opening.as_bytes()[0];
                let length = opening.bytes().take_while(|&byte| byte == mark).count();
                Some(&opening[..length])
            } else {
                None
            }
        })
}

/// The native transcript reads HTML as literal text except for line breaks.
/// Mask the other tags in the probe so they cannot hide a fence, while
/// retaining offsets into the original text.
#[cfg(feature = "host")]
fn without_html_blocks(section: &str) -> std::borrow::Cow<'_, str> {
    if !section.contains('<') {
        return section.into();
    }
    let mut probe = None;
    let mut offset = 0;
    for line in section.split_inclusive(['\r', '\n']) {
        let unindented = line.trim_start_matches(' ');
        let indent = line.len() - unindented.len();
        if indent < 4 && unindented.starts_with('<') && !starts_html_line_break(unindented) {
            let at = offset + indent;
            probe
                .get_or_insert_with(|| section.to_owned())
                .replace_range(at..at + 1, "x");
        }
        offset += line.len();
    }
    match probe {
        Some(probe) => probe.into(),
        None => section.into(),
    }
}

#[cfg(feature = "host")]
fn starts_html_line_break(line: &str) -> bool {
    line.get(..3)
        .is_some_and(|tag| tag.eq_ignore_ascii_case("<br"))
        && {
            let tail = line[3..].trim_start_matches(' ');
            tail.starts_with('>') || tail.starts_with("/>")
        }
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

/// The registry's key for delivered frames. Retained actors receive a legacy
/// verifier at authenticated bootstrap and new delivery evidence with input.
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
        Recognizer::new(self.0)
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

    /// Queued message runs keep their legacy format on disk so an older
    /// daemon can dispatch them after rollback. Strip a verified seal only
    /// at the provider boundary.
    pub fn unseal_run(&self, text: &str) -> Option<FramedPrompt> {
        let (framed, _) = text.rsplit_once(SEAL)?;
        self.recognizer().open(text)?;
        let (body, reply) = framed.rsplit_once('\n')?;
        let text = match reply.strip_prefix(EARLIER_REPLY) {
            Some(command) => format!("{body}\n{REPLY}{command}]"),
            None => format!("{framed}]"),
        };
        let len = text.len() as u32;
        Some(FramedPrompt {
            text,
            lengths: vec![len],
        })
    }

    /// Key a text digest to its recipient. Hashing the prompt first lets
    /// replay check several verified predecessors without rereading its body.
    pub fn delivery_key(&self, recipient: TerminalId, digest: &[u8; 32]) -> [u8; 32] {
        use hmac::Mac;
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.0)
            .expect("HMAC accepts a key of any length");
        mac.update(b"cagents delivered frame\0");
        mac.update(recipient.as_uuid().as_bytes());
        mac.update(digest);
        mac.finalize().into_bytes().into()
    }

    fn sealed(&self, framed: String) -> String {
        let tag = self.tag(&framed);
        format!("{framed}{SEAL}{tag}]")
    }
}

#[cfg(feature = "host")]
pub trait DeliveryLookup: Send + Sync {
    fn lengths(&self, digest: &[u8; 32]) -> Option<Vec<u32>>;
    fn replay(&self) -> Result<DeliveryReplay, String>;
}

#[cfg(feature = "host")]
#[derive(Default)]
pub struct DeliveryReplay {
    pub recipients: Vec<TerminalId>,
    pub records: Vec<FrameEvidence>,
}

#[cfg(feature = "host")]
enum Evidence {
    LegacyOnly,
    Durable(Box<dyn DeliveryLookup>),
    Retained {
        recipients: Vec<TerminalId>,
        frames: std::sync::Mutex<std::collections::HashMap<[u8; 32], Vec<u32>>>,
    },
}

/// Classifies provider text by legacy MAC or evidence in the current scope.
#[cfg(feature = "host")]
pub struct Recognizer {
    key: [u8; 32],
    evidence: Evidence,
}

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
    pub(crate) fn into_recognizer(self, recipients: Vec<TerminalId>) -> Recognizer {
        Recognizer {
            key: self.0,
            evidence: Evidence::Retained {
                recipients,
                frames: Default::default(),
            },
        }
    }
}

#[cfg(feature = "host")]
impl Seals for Recognizer {
    type Running = hmac::Hmac<sha2::Sha256>;

    fn start(&self) -> Self::Running {
        use hmac::Mac;
        hmac::Hmac::new_from_slice(&self.key).expect("HMAC accepts a key of any length")
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
    fn new(key: [u8; 32]) -> Self {
        Self {
            key,
            evidence: Evidence::LegacyOnly,
        }
    }

    pub fn with_delivery_lookup(mut self, lookup: impl DeliveryLookup + 'static) -> Self {
        self.evidence = Evidence::Durable(Box::new(lookup));
        self
    }

    pub(crate) fn delivery_replay(&self) -> Result<DeliveryReplay, String> {
        match &self.evidence {
            Evidence::Durable(lookup) => lookup.replay(),
            Evidence::LegacyOnly | Evidence::Retained { .. } => Ok(DeliveryReplay::default()),
        }
    }

    /// Set by authenticated bootstrap history or a retained input admitted
    /// by its adapter. The provider gets only the text.
    pub(crate) fn record_retained(&self, evidence: impl IntoIterator<Item = FrameEvidence>) {
        if let Evidence::Retained { frames, .. } = &self.evidence {
            frames.lock().unwrap_or_else(|p| p.into_inner()).extend(
                evidence
                    .into_iter()
                    .map(|entry| (entry.digest, entry.lengths)),
            );
        }
    }

    /// Delegate legacy verification to the trusted retained actor.
    pub(crate) fn delegate_to_retained_owner(&self) -> RetainedRecognition {
        RetainedRecognition(self.key)
    }

    fn open(&self, text: &str) -> Option<Opened> {
        open(text, |framed, seal| {
            let mut running = self.start();
            self.extend(&mut running, framed);
            self.accepts(running, seal)
        })
    }

    pub(crate) fn open_all(&self, text: &str) -> Option<Vec<Opened>> {
        if !looks_like_recorded(text) {
            return open_all(text, self);
        }
        let text = text.strip_suffix('\n').unwrap_or(text);
        let digest = digest(text);
        match &self.evidence {
            Evidence::LegacyOnly => None,
            Evidence::Durable(lookup) => open_recorded(text, &lookup.lengths(&digest)?),
            Evidence::Retained { recipients, frames } => {
                let frames = frames.lock().unwrap_or_else(|p| p.into_inner());
                let seal = Seal::new(self.key);
                let lengths = recipients
                    .iter()
                    .find_map(|&recipient| frames.get(&seal.delivery_key(recipient, &digest)))?;
                open_recorded(text, lengths)
            }
        }
    }

    /// A user prompt the daemon sealed, as the delivery a window shows.
    /// Whether it was one.
    pub fn recognize(&self, item: &mut crate::chat::Item) -> bool {
        if let Some(content) = self.recognized_content(item) {
            item.content = content;
            true
        } else {
            false
        }
    }

    /// A recognized delivery from a borrowed transcript item.
    pub fn recognized(&self, item: &crate::chat::Item) -> Option<crate::chat::Item> {
        let content = self.recognized_content(item)?;
        Some(crate::chat::Item {
            id: item.id.clone(),
            role: item.role,
            content,
            started_at: item.started_at,
            at: item.at,
        })
    }

    fn recognized_content(&self, item: &crate::chat::Item) -> Option<crate::chat::ItemContent> {
        if !may_be_frame(item) {
            return None;
        }
        let crate::chat::ItemContent::Text { text, .. } = &item.content else {
            return None;
        };
        self.open_all(text).map(delivery)
    }

    /// Opens the frames among `snapshot`'s items, copying only those it
    /// opens. An opened message keeps no copy source, as one a current
    /// helper opens has none: its source would copy the frame back.
    pub(crate) fn recognize_snapshot(&self, snapshot: &mut crate::chat::Snapshot) {
        for (index, item) in snapshot.items.iter_mut().enumerate() {
            if let Some(opened) = self.recognized(item) {
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
                    if let crate::chat::presentation::ItemChange::Replace(item) = change
                        && self.recognize(item)
                        && let Some(index) = update.order.iter().position(|id| id == &item.id)
                        && let Some(source) = update
                            .metadata
                            .text_copy_sources
                            .conversation
                            .get_mut(index)
                    {
                        *source = None;
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
    fn recorded_boundaries_keep_quoted_frames_in_the_message_body() {
        let nested = FramedPrompt::batch([&message()], 0).text;
        let mut first = message();
        first.body = format!("Here is a quoted prompt 🎉\n\n{nested}");
        let second = message();
        let prompt = FramedPrompt::batch([&first, &second], 0);
        let seal = Seal::new([7; 32]);
        let recipient = TerminalId::new();
        let recognizer = seal
            .recognizer()
            .delegate_to_retained_owner()
            .into_recognizer(vec![recipient]);

        assert!(recognizer.open_all(&prompt.text).is_none());
        recognizer.record_retained([prompt.evidence(&seal, recipient)]);
        let opened = recognizer.open_all(&format!("{}\n", prompt.text)).unwrap();
        assert_eq!(opened.len(), 2);
        assert_eq!(opened[0].body, first.body);
        assert_eq!(opened[1].body, second.body.trim_end());
        assert!(recognizer.open_all(&nested).is_none());
        assert!(
            recognizer
                .open_all(&prompt.text.replace("🎉", "📝"))
                .is_none()
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn a_waiting_run_keeps_its_stored_seal_but_sends_only_the_frame() {
        let seal = Seal::new([7; 32]);
        let mut question = message();
        question.wait_until_ms = Some(10);
        let stored = seal.seal_run(&question);
        let prompt = seal.unseal_run(&stored).unwrap();
        assert_eq!(prompt.text, FramedPrompt::batch([&question], 10).text);
        let earlier = seal.sealed(framed(&question, false).replace(REPLY, EARLIER_REPLY));
        assert_eq!(seal.unseal_run(&earlier).unwrap().text, prompt.text);
        assert!(
            seal.unseal_run(&stored.replace("Hold off", "Go ahead"))
                .is_none()
        );
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
        let sections: Vec<&str> = body.split(BATCH_SEPARATOR).collect();
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
    fn a_delivery_of_several_says_how_many_ask_for_a_reply() {
        use crate::chat::ItemContent;
        let opened = |summary: &str| Opened {
            from: "Review".into(),
            sender: String::new(),
            summary: summary.into(),
            body: "Body".into(),
        };
        let summary = |frames| match delivery(frames) {
            ItemContent::Delivery { summary, .. } => summary,
            content => panic!("{content:?}"),
        };
        assert_eq!(
            summary(vec![opened(""), opened(ASKS)]),
            "2 messages from Review · 1 asks for a reply"
        );
        assert_eq!(
            summary(vec![
                opened(ASKS),
                opened(ASKS),
                opened("Reply to 1234abcd")
            ]),
            "3 messages from Review · 2 ask for a reply"
        );
        assert_eq!(
            summary(vec![opened(""), opened("")]),
            "2 messages from Review"
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn markdown_blocks_end_before_the_next_delivery() {
        use crate::chat::ItemContent;
        use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag};

        let seal = Seal::new([8; 32]);
        let recognizer = seal.recognizer();
        let mut first = message();
        let mut second = message();
        second.body = "# Landed\n\nThe fix is on main.".into();
        for (source, closer) in [
            ("Here is the failing part:\n```rust\nfn main() {", "\n```"),
            ("  ~~~~\n~~~\nstill code", "\n~~~~"),
            ("```sh\n``` not a close\n````", ""),
            ("``` not `a` fence", ""),
            ("    ```\nindented code", ""),
            ("- List item\n  ```rust\n  fn main() {", ""),
            ("- List item\n\t```rust\n\tfn main() {", ""),
            ("> Quote\n> ```rust\n> fn main() {", ""),
            ("<div>\nNormal HTML block", ""),
            ("  <PRE class=example>\nUnclosed HTML", ""),
            ("<!--\nUnclosed comment", ""),
            ("Before\r<!--\rUnclosed comment", ""),
            ("<!-- note\n```rust\nlet x = 1;", "\n```"),
            ("<pre>\n```text\nA visible code block", "\n```"),
            ("<br>\n```text\n```\t\n\nAfter the blank.", ""),
            ("<BR />\n```text\ncode\n\n```", "\n```"),
        ] {
            first.body = source.into();
            let opened = recognizer
                .open_all(&seal.seal_all([&first, &second], 0))
                .unwrap();
            assert_eq!(opened[0].body, source);
            let ItemContent::Delivery { body, .. } = delivery(opened) else {
                panic!("a delivery");
            };
            let (first, second) = body.split_once(BATCH_SEPARATOR).unwrap();
            assert!(first.ends_with(&format!("{source}{closer}")), "{body}");
            assert!(
                second.ends_with("# Landed\n\nThe fix is on main."),
                "{body}"
            );
            let probe = without_html_blocks(&body);
            let mut events = Parser::new_ext(&probe, Options::ENABLE_TABLES);
            assert!(events.any(|event| event == Event::Rule), "{body}");
            assert!(
                events.any(|event| matches!(
                    event,
                    Event::Start(Tag::Heading {
                        level: HeadingLevel::H1,
                        ..
                    })
                )),
                "{body}"
            );
        }
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
