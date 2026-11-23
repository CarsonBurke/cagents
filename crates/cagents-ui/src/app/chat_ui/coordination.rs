//! Drawing what an agent did to work with other agents.
//!
//! A message one agent sends another, and an agent one starts, is routine
//! activity, so each is one row of the step list: "Message to Relocate the
//! repo", the start of what it said, and how it went. The row opens the
//! agent; its tooltip gives the start of the whole message, and its context
//! menu copies it and opens the row's detail: the message in full, what came
//! back, and the command or tool call behind it. What the agent did is in its
//! transcript. Every other coordinating step stays one line of the
//! step strip, but says what it did - "Waited for reviewer" - rather than
//! reprinting the command that did it.

use super::*;
use crate::agent_execution::State;
use crate::chat::ToolStatus;
use crate::chat::coordination::{
    Act, Arrival, Coordination, Delivery, Kind, Outbound, OutboundKind, Run, Spawn, Text,
};

#[cfg(test)]
thread_local! { static PREPARATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(super) fn preparations() -> usize {
    PREPARATIONS.with(std::cell::Cell::get)
}
/// Send payload formed by the admitted parser worker, including every rich
/// part and its immutable publication graph. UI owners transfer it intact on
/// eviction, so canonical parser and recognition metadata also retire off UI.
pub(super) struct Prepared {
    pub(super) reading: Coordination,
    prompts: Vec<Option<markdown::Content>>,
    brief: Option<markdown::Content>,
    answers: Vec<markdown::Content>,
    documents: Vec<prose_document::Data>,
    /// What a step drawn as one row says, read once here rather than on
    /// every view.
    pub(super) plain: Plain,
    #[cfg(test)]
    pub(super) created_on: std::thread::ThreadId,
    #[cfg(test)]
    disposed: Option<std::sync::mpsc::Sender<std::thread::ThreadId>>,
}
impl Prepared {
    /// The pull requests the texts this card parses link to or mention.
    pub(super) fn links(&self) -> Vec<super::pull_request_links::Named> {
        let reading = &self.reading;
        let prompts: Vec<&str> = match &reading.kind {
            Kind::Message(message) => message.body.inline().into_iter().collect(),
            Kind::Spawn(spawn) => std::iter::once(spawn.brief.as_str())
                .chain(
                    spawn
                        .agents
                        .iter()
                        .filter_map(|agent| agent.prompt.inline()),
                )
                .collect(),
            Kind::Act(_) => Vec::new(),
        };
        prompts
            .into_iter()
            .chain(
                reading
                    .outcome
                    .messages
                    .iter()
                    .map(|message| message.body.as_str()),
            )
            .chain(reading.outcome.run.iter().map(|run| run.result.as_str()))
            .flat_map(super::pull_request_links::named)
            .collect()
    }

    pub(super) fn prepare(
        reading: Coordination,
        labels: &Arc<super::pull_request_links::Labels>,
        cancelled: &AtomicBool,
    ) -> Result<Self, String> {
        let check = || {
            if cancelled.load(Ordering::Relaxed) {
                Err("Message loading was cancelled.".to_owned())
            } else {
                Ok(())
            }
        };
        check()?;
        #[cfg(test)]
        PREPARATIONS.with(|count| count.set(count.get() + 1));
        let parse = |text: &str| {
            check()?;
            Ok(markdown::Content::parse_labelled(text, labels.clone()))
        };
        let inline = |text: &Text| text.inline().map(parse).transpose();
        // A step drawn as one row reads its text plain, in the row and its
        // tooltip, and formatted in its open detail.
        let plain = if reading.is_card() {
            check()?;
            Plain::of(&reading)
        } else {
            Plain::default()
        };
        let prompts = match &reading.kind {
            Kind::Message(message) => vec![inline(&message.body)?],
            Kind::Spawn(spawn) => spawn
                .agents
                .iter()
                .map(|agent| inline(&agent.prompt))
                .collect::<Result<Vec<_>, String>>()?,
            Kind::Act(_) => Vec::new(),
        };
        let brief = match &reading.kind {
            Kind::Spawn(spawn) if !spawn.brief.trim().is_empty() => Some(parse(&spawn.brief)?),
            _ => None,
        };
        let mut answers: Vec<markdown::Content> = reading
            .outcome
            .messages
            .iter()
            .map(|message| parse(&message.body))
            .collect::<Result<Vec<_>, String>>()?;
        if let Some(run) = &reading.outcome.run
            && !run.result.is_empty()
        {
            answers.push(parse(&run.result)?);
        }
        let mut prepared = Self {
            reading,
            prompts,
            brief,
            answers,
            documents: vec![],
            plain,
            #[cfg(test)]
            created_on: std::thread::current().id(),
            #[cfg(test)]
            disposed: None,
        };
        for content in prepared
            .prompts
            .iter_mut()
            .flatten()
            .chain(&mut prepared.brief)
            .chain(&mut prepared.answers)
        {
            content.discard_continuation();
            preparation::prime_plain(content.items(), cancelled)?;
        }
        prepared.documents = prepared
            .contents()
            .map(|content| {
                check()?;
                Ok(prose_document::Data::snapshot(content))
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(prepared)
    }
    #[cfg(test)]
    pub(super) fn new(reading: Coordination) -> Self {
        Self::prepare(reading, &Arc::default(), &AtomicBool::new(false)).unwrap()
    }
    pub(super) fn contents(&self) -> impl Iterator<Item = &markdown::Content> {
        self.prompts
            .iter()
            .flatten()
            .chain(&self.brief)
            .chain(&self.answers)
    }
    #[cfg(test)]
    pub(super) fn watch_disposal(
        &mut self,
        sender: std::sync::mpsc::Sender<std::thread::ThreadId>,
    ) {
        self.disposed = Some(sender);
    }
}
impl Drop for Prepared {
    fn drop(&mut self) {
        // Raw Send graphs drop inside one existing reclaimer batch. UI owners
        // transfer this complete payload; no recursive retirement is queued.
        drop(std::mem::take(&mut self.prompts));
        drop(self.brief.take());
        drop(std::mem::take(&mut self.answers));
        drop(std::mem::take(&mut self.documents));
        #[cfg(test)]
        if let Some(disposed) = self.disposed.take() {
            let _ = disposed.send(std::thread::current().id());
        }
    }
}
pub(super) struct Coordinated {
    prepared: Option<Prepared>,
    documents: Vec<prose_document::Document>,
}
impl std::ops::Deref for Coordinated {
    type Target = Prepared;
    fn deref(&self) -> &Prepared {
        self.prepared.as_ref().expect("live coordination")
    }
}
impl Drop for Coordinated {
    fn drop(&mut self) {
        if let Some(prepared) = self.prepared.take() {
            preparation::retire_coordination(prepared);
        }
    }
}
impl Coordinated {
    pub(super) fn from_prepared(mut prepared: Prepared) -> Self {
        let documents = std::mem::take(&mut prepared.documents)
            .into_iter()
            .map(prose_document::Document::new)
            .collect();
        Self {
            prepared: Some(prepared),
            documents,
        }
    }
    #[cfg(test)]
    pub(super) fn new(reading: Coordination) -> Self {
        Self::from_prepared(Prepared::new(reading))
    }

    /// Every piece of Markdown the card draws, for highlighting its fences.
    pub(super) fn contents(&self) -> impl Iterator<Item = &markdown::Content> {
        self.prompts
            .iter()
            .flatten()
            .chain(&self.brief)
            .chain(&self.answers)
    }

    #[cfg(test)]
    pub(super) fn address(&self) -> usize {
        self.prepared.as_ref().expect("live coordination") as *const Prepared as usize
    }
    pub(super) fn document(
        &self,
        content: &markdown::Content,
    ) -> Option<&prose_document::Document> {
        self.contents()
            .position(|candidate| std::ptr::eq(candidate, content))
            .and_then(|index| self.documents.get(index))
    }

    pub(super) fn is_card(&self) -> bool {
        self.reading.is_card()
    }

    /// Whether the step is a message to another agent, which folds with the
    /// messages beside it.
    pub(super) fn is_message(&self) -> bool {
        matches!(self.reading.kind, Kind::Message(_))
    }

    /// What copying the card takes: the message, or the prompt.
    pub(super) fn copy_text(&self) -> Option<String> {
        match &self.reading.kind {
            Kind::Message(message) => message.body.inline().map(str::to_owned),
            Kind::Spawn(spawn) => {
                let prompts: Vec<&str> = spawn
                    .agents
                    .iter()
                    .filter_map(|agent| agent.prompt.inline())
                    .collect();
                (!prompts.is_empty()).then(|| prompts.join("\n\n"))
            }
            Kind::Act(_) => None,
        }
    }

    /// The step's glyph, where it is not the one its item's kind has.
    pub(super) fn glyph(&self) -> Option<Icon> {
        Some(match &self.reading.kind {
            Kind::Message(_) => Icon::Send,
            Kind::Spawn(_) => Icon::Agent,
            Kind::Act(Act::Inbox { .. }) => Icon::Chat,
            Kind::Act(Act::Linear { .. }) => Icon::Comment,
            Kind::Act(Act::Sessions | Act::Whoami | Act::Agents) => Icon::Users,
            Kind::Act(Act::Stop { .. } | Act::Output { .. } | Act::Cli(_)) => return None,
            Kind::Act(_) => Icon::Agent,
        })
    }

    /// The step's verb and what it acted on, `listed` naming an agent this
    /// conversation lists, by its listed ID or its provider's address for it.
    pub(super) fn line(&self, listed: &dyn Fn(&str) -> Option<String>) -> (String, String) {
        let reading = &self.reading;
        let names = |agents: &[String]| agents_phrase(reading, agents, listed);
        let name = |agent: &String| names(std::slice::from_ref(agent));
        let (verb, label): (&str, String) = match &reading.kind {
            Kind::Message(message) => (
                message_verb(message.kind),
                recipient(message, reading, listed).plain(),
            ),
            // The started agent goes by its tab's name once it has one.
            Kind::Spawn(spawn) => (
                "Started",
                match self.reading.subagents.as_slice() {
                    [id] => listed(id),
                    _ => None,
                }
                .unwrap_or_else(|| started(spawn)),
            ),
            Kind::Act(act) => match act {
                Act::Sessions => ("Listed", "live sessions".into()),
                Act::Whoami => ("Looked up", "this session".into()),
                Act::Inbox { waits: false } => ("Checked", "the inbox".into()),
                Act::Inbox { waits: true } => ("Waited for", "a message".into()),
                Act::Agents => ("Listed", "its agents".into()),
                Act::Status { agent } => ("Checked on", name(agent)),
                // Codex names the agents waited on only once the wait ends.
                Act::Wait { agents } if agents.is_empty() => ("Waited for", "agents".into()),
                Act::Wait { agents } => ("Waited for", names(agents)),
                Act::Requests { agent } => ("Read what", format!("{} is asking", name(agent))),
                Act::Answer {
                    agent,
                    request,
                    action,
                } => (
                    "Answered",
                    format!("{}'s request {request} with {action}", name(agent)),
                ),
                Act::Interrupt { agent } => ("Interrupted", name(agent)),
                Act::Close { agents } => ("Closed", names(agents)),
                Act::Resume { agents } => ("Resumed", names(agents)),
                // A task is an agent where this conversation lists one by it,
                // and otherwise a shell, whose ID says nothing to a reader.
                Act::Stop { task } => (
                    "Stopped",
                    listed(task).unwrap_or_else(|| "a background task".into()),
                ),
                Act::Output { task } => (
                    "Read the output of",
                    listed(task).unwrap_or_else(|| "a background task".into()),
                ),
                Act::Linear { verb, subject } => (verb, subject.clone()),
                Act::Cli(path) => ("Ran", format!("cagents {path}").trim_end().to_owned()),
            },
        };
        (verb.to_owned(), label)
    }

    /// The step's line where the conversation's subagents are not to hand,
    /// for a summary formed without them.
    pub(super) fn summary(&self) -> String {
        self.summary_naming(&|_| None)
    }

    /// The step's line as one string, `listed` naming the agents this
    /// conversation lists.
    pub(super) fn summary_naming(&self, listed: &dyn Fn(&str) -> Option<String>) -> String {
        let (verb, label) = self.line(listed);
        if label.is_empty() {
            verb
        } else {
            format!("{verb} {label}")
        }
    }
}

fn message_verb(kind: OutboundKind) -> &'static str {
    match kind {
        OutboundKind::Send => "Message to",
        OutboundKind::Ask => "Question to",
        OutboundKind::Reply => "Reply to",
        OutboundKind::Prompt => "Follow-up to",
        OutboundKind::Report => "Report to",
    }
}

/// An ID's first eight characters, which is how the CLI prints one. A name,
/// such as `reviewer-2`, is kept whole.
fn short_id(id: &str) -> &str {
    if !is_identifier(id) {
        return id;
    }
    id.char_indices().nth(8).map_or(id, |(end, _)| &id[..end])
}

/// Whom started agents are, as one phrase. One agent goes by its name, else
/// its task's first line, else its type, as its tab will name it.
fn started(spawn: &Spawn) -> String {
    match spawn.agents.as_slice() {
        [only] => [&only.name, &only.description, &only.kind]
            .into_iter()
            .find_map(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
            .map_or_else(|| "an agent".to_owned(), str::to_owned),
        agents => format!("{} agents", agents.len()),
    }
}

/// A phrase for `agents`, as a step names them: by the names their tabs show
/// where this conversation lists them. A `cagents` agent it does not list goes
/// by the short ID the CLI prints; a provider's own agent, whose IDs the app
/// shows nowhere, is only counted.
fn agents_phrase(
    reading: &Coordination,
    agents: &[String],
    listed: &dyn Fn(&str) -> Option<String>,
) -> String {
    let mut names = Vec::new();
    let mut unknown = 0;
    for agent in agents {
        match listed(agent) {
            Some(name) => names.push(name),
            None if reading.cli || !is_identifier(agent) => names.push(short_id(agent).to_owned()),
            None => unknown += 1,
        }
    }
    let names = names.join(", ");
    match (names.is_empty(), unknown) {
        (true, 0) => "agents".to_owned(),
        (true, 1) => "an agent".to_owned(),
        (true, count) => format!("{count} agents"),
        (false, 0) => names,
        (false, 1) => format!("{names} and another agent"),
        (false, count) => format!("{names} and {count} other agents"),
    }
}

/// A message's recipient: a name, and the session ID that tells it apart.
struct Recipient {
    name: String,
    /// A session's ID or a prefix of one.
    session: Option<String>,
    /// Whether the session runs now, so the reader can go to it.
    live: bool,
}

impl Recipient {
    fn plain(&self) -> String {
        match (&self.session, self.name.is_empty()) {
            (Some(session), true) => format!("session {}", short_id(session)),
            (Some(session), false) => format!("{} ({})", self.name, short_id(session)),
            (None, _) => self.name.clone(),
        }
    }

    /// As a row draws it: the name cut to fit beside the session ID that
    /// tells it apart, and that ID, which the row keeps whole after it.
    fn shown(&self) -> (String, String) {
        let cut = |name: &str, room: usize| Runtime::short_description(name, room);
        match (&self.session, self.name.is_empty()) {
            (Some(session), false) => {
                let id = short_id(session);
                let room = LABEL_CHARS.saturating_sub(id.chars().count() + 3).max(8);
                (cut(&self.name, room), format!(" ({id})"))
            }
            _ => (cut(&self.plain(), LABEL_CHARS), String::new()),
        }
    }
}

/// Whom `message` went to, by the CLI's own account where it printed one
/// and otherwise as the agent named them.
fn recipient(
    message: &Outbound,
    reading: &Coordination,
    listed: &dyn Fn(&str) -> Option<String>,
) -> Recipient {
    if let Some(receipt) = &reading.outcome.receipt {
        // `session 95fcd1b9 "Title"`, `session 95fcd1b9`, or `the user`.
        if let Some(rest) = receipt.recipient.strip_prefix("session ") {
            let (session, title) = rest.split_once(' ').unwrap_or((rest, ""));
            return Recipient {
                name: title.trim_matches('"').to_owned(),
                session: Some(session.to_owned()),
                // An offline recipient is named by its lineage, which no
                // open pane has.
                live: receipt.arrival != Arrival::NextSession,
            };
        }
        return Recipient {
            name: receipt.recipient.clone(),
            session: None,
            live: false,
        };
    }
    let named = |name: String| Recipient {
        name,
        session: None,
        live: false,
    };
    let phrase = |agents: &[String]| named(agents_phrase(reading, agents, listed));
    match message.kind {
        OutboundKind::Report => named("the conversation that started it".into()),
        OutboundKind::Reply => named(format!("message {}", short_id(&message.to))),
        _ if message.to == "parent" => named("the session that started it".into()),
        // A `cagents` recipient is a session ID, or a prefix of one.
        _ if reading.cli && is_identifier(&message.to) => Recipient {
            name: String::new(),
            session: Some(message.to.clone()),
            live: true,
        },
        // The agents a provider's own tool reached, which a name the agent
        // gave them stands in for until the tool says which.
        _ if !reading.subagents.is_empty() => match reading.subagents.as_slice() {
            [agent] if listed(agent).is_none() && !is_identifier(&message.to) => {
                named(message.to.clone())
            }
            agents => phrase(agents),
        },
        _ if message.to == "*" => named("every teammate".into()),
        _ if message.to.contains(':') => named("another session".into()),
        _ if message.to.is_empty() => named("an agent".into()),
        _ => phrase(std::slice::from_ref(&message.to)),
    }
}

fn is_identifier(word: &str) -> bool {
    word.len() >= 4 && word.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// How `item`'s step went, as its reading reports it, and what it printed.
fn reported<'a>(item: &'a chat::Item, coordinated: &Coordinated) -> (ToolStatus, &'a str) {
    let reported = item.content.status().unwrap_or(ToolStatus::Succeeded);
    let output = match &item.content {
        ItemContent::Command { output, .. } | ItemContent::Tool { output, .. } => output.as_str(),
        _ => "",
    };
    (coordinated.reading.status(reported), output)
}

/// Messages to other agents one after another, as the one row they fold
/// into: whom they went to, what they said, and how they stand together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MessageRun {
    verb: &'static str,
    /// How many: "11 agents" where each went to its own with the same
    /// words, otherwise "3 messages".
    count: String,
    /// The words they share, or else whom they went to, and the words when
    /// one agent was sent them again.
    preview: String,
    /// Whether the preview is the words each agent was sent.
    shared: bool,
    state: RowStatus,
    /// Whom each message went to and how it stands, for the tooltip.
    each: Vec<String>,
    /// The whole of the words they share, for the tooltip and to copy.
    full: Option<String>,
    /// The first message, whose words copying takes when they share them.
    first: String,
}

/// One agent a run of messages went to: every name a send to it went by,
/// and the one the reader knows it by.
struct Agent {
    names: Vec<String>,
    shown: String,
}

impl Agent {
    /// The one agent a message reached, by every name the send went by:
    /// what it was addressed to, the agent the provider resolved, and the
    /// session the CLI reports.
    fn sent(
        message: &Outbound,
        reading: &Coordination,
        listed: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        let mut names = vec![message.to.clone()];
        for agent in &reading.subagents {
            names.push(agent.clone());
            names.extend(listed(agent));
        }
        if let Some(receipt) = &reading.outcome.receipt {
            match receipt.recipient.strip_prefix("session ") {
                Some(rest) => {
                    let (session, title) = rest.split_once(' ').unwrap_or((rest, ""));
                    names.push(session.to_owned());
                    names.push(title.trim_matches('"').to_owned());
                }
                None => names.push(receipt.recipient.clone()),
            }
        }
        names.retain(|name| !name.is_empty());
        Self {
            names,
            shown: recipient(message, reading, listed).plain(),
        }
    }

    /// Whether `other` is this agent: by a session or agent ID, matching
    /// any of its abbreviations, where both know one, since sessions that
    /// share a title are not one; otherwise by any name of theirs.
    fn is(&self, other: &Agent) -> bool {
        let ids = |agent: &Agent| {
            agent
                .names
                .iter()
                .filter(|name| is_identifier(name))
                .cloned()
                .collect::<Vec<_>>()
        };
        let (mine, theirs) = (ids(self), ids(other));
        if !mine.is_empty() && !theirs.is_empty() {
            return mine.iter().any(|id| {
                theirs
                    .iter()
                    .any(|their| id.starts_with(their.as_str()) || their.starts_with(id.as_str()))
            });
        }
        self.names.iter().any(|name| other.names.contains(name))
    }
}

impl MessageRun {
    /// The row `sent` folds into, `listed` naming the agents this
    /// conversation lists; `None` unless every step is a message.
    pub(super) fn of<'a>(
        sent: impl IntoIterator<Item = (&'a chat::Item, &'a Coordinated)>,
        listed: &dyn Fn(&str) -> Option<String>,
    ) -> Option<Self> {
        let mut bodies = Vec::new();
        let mut agents: Vec<Agent> = Vec::new();
        let mut states = Vec::new();
        let mut each = Vec::new();
        let mut first = None;
        // How many agents the messages reached between them, and whether
        // one went to every teammate, which is more than this can count.
        let (mut reached, mut everyone) = (0, false);
        for (item, coordinated) in sent {
            let reading = &coordinated.reading;
            let Kind::Message(message) = &reading.kind else {
                return None;
            };
            first.get_or_insert_with(|| item.id.clone());
            let (status, output) = reported(item, coordinated);
            let state = message_state(reading, message, status, output);
            each.push(format!(
                "{}: {}",
                recipient(message, reading, listed).plain(),
                state.reason()
            ));
            states.push(state);
            // Compared whole, as the agent wrote them: messages that open
            // alike and differ further on are not one message.
            bodies.push(
                message
                    .body
                    .inline()
                    .map(|body| (body, coordinated.plain.said.first())),
            );
            everyone |= message.to == "*" && reading.subagents.is_empty();
            let sends: Vec<Agent> = match reading.subagents.as_slice() {
                // One message to several agents reached each of them.
                several @ [_, _, ..] => several
                    .iter()
                    .map(|agent| Agent {
                        names: std::iter::once(agent.clone())
                            .chain(listed(agent))
                            .collect(),
                        shown: agents_phrase(reading, std::slice::from_ref(agent), listed),
                    })
                    .collect(),
                _ => vec![Agent::sent(message, reading, listed)],
            };
            reached += sends.len();
            for mut agent in sends {
                // The latest send to an agent names it best: it reached it.
                agents.retain_mut(|known| {
                    let same = known.is(&agent);
                    if same {
                        agent.names.append(&mut known.names);
                    }
                    !same
                });
                agents.push(agent);
            }
        }
        let count = bodies.len();
        // The same words to several is one message said to each of them,
        // which is what the reader needs from the row; different words are
        // told apart by whom they went to, each message's own row giving
        // its words.
        let full = match bodies.split_first() {
            Some((Some((first, said)), rest)) if !first.trim().is_empty() => rest
                .iter()
                .all(|body| body.is_some_and(|(body, _)| body == *first))
                .then(|| said.cloned().unwrap_or_default())
                .filter(|said| !said.trim().is_empty()),
            _ => None,
        };
        let shared = full.as_deref().map(one_line_of);
        let distinct = !everyone && agents.len() == reached;
        let mut recipients: Vec<&str> = Vec::new();
        for agent in &agents {
            if !recipients.contains(&agent.shown.as_str()) {
                recipients.push(&agent.shown);
            }
        }
        // Agents this conversation cannot tell apart by name are counted.
        let to = if recipients.len() < agents.len() {
            format!("to {} agents", agents.len())
        } else {
            format!("to {}", recipients.join(", "))
        };
        let (verb, count, preview, shares) = match shared {
            Some(said) if distinct => ("Messaged", format!("{} agents", agents.len()), said, true),
            // The same words sent again, as a retry is, or to every
            // teammate, still say whom to.
            Some(said) => (
                "Sent",
                format!("{count} messages"),
                format!("{to}: {said}"),
                false,
            ),
            None => ("Sent", format!("{count} messages"), to, false),
        };
        Some(Self {
            verb,
            count,
            preview,
            shared: shares,
            state: rollup(states),
            each,
            full,
            first: first?,
        })
    }

    /// The tooltip: whom each message went to and how it stands, then the
    /// words they share.
    pub(super) fn tip(&self) -> String {
        const LISTED: usize = 12;
        let mut tip = vec![format!("{} {}", self.verb, self.count)];
        let mut each = self.each.iter().take(LISTED).cloned().collect::<Vec<_>>();
        if self.each.len() > LISTED {
            each.push(format!("and {} more", self.each.len() - LISTED));
        }
        tip.push(each.join("\n"));
        tip.extend(self.full.as_deref().map(bounded));
        tip.join("\n\n")
    }

    /// The row as one line of plain text.
    pub(super) fn line(&self) -> String {
        let joint = if self.shared { ":" } else { "" };
        format!(
            "{} {}{joint} {} · {}",
            self.verb, self.count, self.preview, self.state.text
        )
    }
}

/// How several messages stand together. Where they all stand alike that is
/// the one word their rows say; otherwise each state is counted, those that
/// need the reader first - a message not sent, then one not confirmed, then
/// one still under way - so a row too narrow for all of them keeps those.
fn rollup(states: Vec<RowStatus>) -> RowStatus {
    let rank = |state: &RowStatus| match (state.tone, state.live) {
        (Tone::Error, _) => 0,
        (Tone::Warning, _) => 1,
        (Tone::Plain, true) => 2,
        (Tone::Plain, false) => 3,
    };
    let mut counted: Vec<(RowStatus, usize)> = Vec::new();
    for state in states {
        match counted.iter_mut().find(|(seen, _)| seen.text == state.text) {
            Some((_, count)) => *count += 1,
            None => counted.push((state, 1)),
        }
    }
    // A stable sort keeps states of one rank in the order they first came.
    counted.sort_by_key(|(state, _)| rank(state));
    let tone = counted.first().map_or(Tone::Plain, |(state, _)| state.tone);
    let live = counted.iter().any(|(state, _)| state.live);
    if let [(only, _)] = counted.as_slice() {
        return RowStatus::with(only.text.clone(), tone, live);
    }
    let text = counted
        .iter()
        .map(|(state, count)| {
            let mut words = state.text.chars();
            let first = words.next().map(|first| first.to_lowercase().to_string());
            format!("{count} {}{}", first.unwrap_or_default(), words.as_str())
        })
        .collect::<Vec<_>>()
        .join(" · ");
    RowStatus::with(text, tone, live)
}

/// How a message stands, as its row's status: sending, waiting for an
/// answer, how it arrived, or why it was not sent.
fn message_state(
    reading: &Coordination,
    message: &Outbound,
    status: ToolStatus,
    output: &str,
) -> RowStatus {
    let receipt = reading.outcome.receipt.as_ref();
    let answered = !reading.outcome.messages.is_empty();
    match (status, receipt) {
        (ToolStatus::Running, _) if message.waits => RowStatus::live("Waiting for an answer"),
        (ToolStatus::Running, _) => RowStatus::live("Sending"),
        // An ask that timed out was still sent; only the answer is late.
        (ToolStatus::Failed, Some(receipt)) if message.waits => RowStatus {
            detail: Some(format!(
                "{}. No answer yet; it will arrive as a message.",
                arrival(receipt.arrival)
            )),
            ..RowStatus::warning("No answer yet")
        },
        // An ask prints its receipt only once its wait ends, so one
        // stopped while waiting was sent without saying so.
        (ToolStatus::Stopped, _) if message.waits => RowStatus::warning("Stopped waiting"),
        (ToolStatus::Stopped, None) => RowStatus {
            detail: Some("Stopped before it confirmed sending.".to_owned()),
            ..RowStatus::warning("Not confirmed")
        },
        (ToolStatus::Failed | ToolStatus::Denied | ToolStatus::Stopped, _) => RowStatus {
            detail: Some(failure(reading, output).to_owned()).filter(|why| !why.is_empty()),
            ..RowStatus::error("Not sent")
        },
        _ if answered => RowStatus::plain("Answered"),
        (_, Some(receipt)) => received(receipt.arrival),
        _ => match (reading.outcome.delivery, reading.outcome.run.as_ref()) {
            (Some(delivery), _) => RowStatus {
                detail: Some(delivered(delivery).to_owned()),
                ..RowStatus::plain(match delivery {
                    Delivery::Queued => "Queued",
                    Delivery::Resumed => "Resumed",
                    Delivery::Inbox | Delivery::Sent => "Delivered",
                })
            },
            // A prompt to an agent starts a run of it.
            (None, Some(run)) => run_status(run),
            (None, None) => RowStatus::plain("Delivered"),
        },
    }
}

/// How a confirmed message reaches its recipient, in the card's status line.
fn arrival(arrival: Arrival) -> &'static str {
    match arrival {
        Arrival::WhenIdle => "Arrives when they are idle",
        Arrival::Inbox => "Left in their inbox",
        Arrival::NextSession => "Held for their next session",
        Arrival::User => "Left in your inbox",
        Arrival::Started => "Started a run for them",
        Arrival::Queued => "Runs after their current work",
        Arrival::Behind => "Runs after messages sent to them earlier",
        Arrival::NoSlot => "Waits for a free agent slot",
        Arrival::Ended => "Ran for them; the run ended",
        Arrival::Unknown => "Run ended; may not have arrived",
    }
}

/// A confirmed message's row status: its word, and the full arrival behind it.
fn received(arrival_of: Arrival) -> RowStatus {
    RowStatus {
        detail: Some(arrival(arrival_of).to_owned()),
        ..match arrival_of {
            Arrival::WhenIdle
            | Arrival::NextSession
            | Arrival::Queued
            | Arrival::Behind
            | Arrival::NoSlot => RowStatus::plain("Queued"),
            // A run that ended had the message all the same.
            Arrival::Inbox | Arrival::User | Arrival::Started | Arrival::Ended => {
                RowStatus::plain("Delivered")
            }
            Arrival::Unknown => RowStatus::warning("Not confirmed"),
        }
    }
}

/// What became of a message a provider's own tool sent, in the card's
/// status line.
fn delivered(delivery: Delivery) -> &'static str {
    match delivery {
        Delivery::Queued => "Queued for its next step",
        Delivery::Resumed => "Resumed it with this message",
        Delivery::Inbox => "Left in that session's inbox",
        Delivery::Sent => "Sent",
    }
}

/// How a started agent's run stands, as a reader would say it.
fn state_phrase(state: State) -> &'static str {
    match state {
        State::Accepted => "Accepted",
        State::Running => "Running",
        State::Waiting => "Waiting for an answer",
        State::Succeeded => "Finished",
        State::Failed => "Failed",
        State::Cancelled => "Cancelled",
        State::Interrupted => "Interrupted",
    }
}

/// Why a step failed: what the CLI said, or else the last thing it printed.
fn failure<'a>(reading: &'a Coordination, output: &'a str) -> &'a str {
    reading
        .outcome
        .error
        .as_deref()
        .unwrap_or_else(|| last_line(output))
}

/// The last thing a command printed.
fn last_line(output: &str) -> &str {
    output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

/// What a launch reported about the agent it started, for the row's tooltip:
/// only what was reported, as native children do not inherit the parent's
/// model merely because they share a provider.
fn spawn_details(reading: &Coordination, spawn: &Spawn, observed_model: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if reading.cli {
        parts.push("cagents agent".to_owned());
    } else if let [agent] = spawn.agents.as_slice()
        && !agent.kind.trim().is_empty()
    {
        parts.push(agent.kind.trim().to_owned());
    }
    if !spawn.provider.is_empty() {
        parts.push(spawn.provider.clone());
    }
    if let Some(model) = observed_model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .or_else(|| Some(spawn.model.trim()).filter(|model| !model.is_empty()))
    {
        parts.push(model.to_owned());
    }
    if !spawn.effort.is_empty() {
        parts.push(spawn.effort.clone());
    }
    if reading.cli {
        match (&spawn.placement, &spawn.channel) {
            (Some(crate::agent_execution::Placement::Channel(channel)), _)
            | (None, Some(channel)) => parts.push(format!("Channel {channel}")),
            (Some(crate::agent_execution::Placement::Worktree { path, .. }), _) => {
                parts.push(format!("Worktree view {}", path.display()))
            }
            (None, None) => {}
        }
    }
    let mut details = parts.join(" · ");
    if !spawn.directory.is_empty() {
        if !details.is_empty() {
            details.push_str(" · ");
        }
        details.push_str("in ");
        details.push_str(&spawn.directory);
    }
    details
}

/// How many characters of a name that is not a listed agent's a row shows:
/// few enough that, in the narrowest chat pane, the status after it keeps
/// its place, and enough for "the conversation that started it". A listed
/// agent's name is bounded by [`ROW_NAME_CHARS`] instead.
const LABEL_CHARS: usize = 32;

/// How much of a listed agent's name its row may show. The row fades it
/// where it runs short; this bounds only the text shaped for it.
const ROW_NAME_CHARS: usize = 80;

/// How much of its status a row shows, a caption naming what an agent waits
/// on included, so that the status leaves room for the agent's name. An
/// agent's caption is already held to it, so the row and tab say the same.
const STATUS_CHARS: usize = super::view::AGENT_CAPTION_CHARS;
/// The room a row keeps at its trailing edge for the open arrow: the glyph
/// and the gap before it, as a step row keeps for its chevron, so statuses
/// end in line with step durations.
const ARROW_ROOM: f32 = design::COMPACT_ICON_SIZE as f32 + step::GAP_S;
/// How much of a message a row's tooltip shows.
const TIP_CHARS: usize = 600;
const TIP_LINES: usize = 12;

/// `text` as one line: its lines joined by spaces, bounded well past what
/// a row can show, which the row then cuts to its width.
fn one_line_of(text: &str) -> String {
    let mut line = String::new();
    for word in text.split_whitespace() {
        if line.len() > 240 {
            break;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    line
}

/// Markdown as the plain text a reader sees in it: its words, list markers
/// and line structure, without the syntax that styles them. Only as much of
/// it is read as a row or its tooltip can show.
fn plain(markdown: &str) -> String {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    let source = match markdown.char_indices().nth(4 * TIP_CHARS) {
        Some((end, _)) => &markdown[..end],
        None => markdown,
    };
    let mut text = String::new();
    // Each open list's next number, or `None` for a bulleted one.
    let mut lists: Vec<Option<u64>> = Vec::new();
    let break_block = |text: &mut String| {
        let kept = text.trim_end_matches([' ', '\n']).len();
        text.truncate(kept);
        if !text.is_empty() {
            text.push_str("\n\n");
        }
    };
    for event in Parser::new(source) {
        match event {
            Event::Text(part) | Event::Code(part) | Event::InlineMath(part) => text.push_str(&part),
            Event::DisplayMath(part) | Event::Html(part) | Event::InlineHtml(part) => {
                text.push_str(&part)
            }
            Event::SoftBreak => text.push(' '),
            Event::HardBreak => text.push('\n'),
            Event::Rule => {
                break_block(&mut text);
                text.push_str("———");
            }
            Event::TaskListMarker(done) => text.push_str(if done { "☑ " } else { "☐ " }),
            Event::Start(Tag::List(first)) => {
                if lists.is_empty() {
                    break_block(&mut text);
                }
                lists.push(first);
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                let kept = text.trim_end_matches(' ').len();
                text.truncate(kept);
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                let depth = lists.len().saturating_sub(1);
                text.push_str(&"  ".repeat(depth));
                match lists.last_mut() {
                    Some(Some(number)) => {
                        text.push_str(&format!("{number}. "));
                        *number += 1;
                    }
                    _ => text.push_str("• "),
                }
            }
            Event::Start(Tag::Paragraph | Tag::Heading { .. } | Tag::BlockQuote(_))
            | Event::Start(Tag::CodeBlock(_) | Tag::Table(_))
                if lists.is_empty() =>
            {
                break_block(&mut text)
            }
            Event::End(TagEnd::TableRow | TagEnd::TableHead) => text.push('\n'),
            Event::End(TagEnd::TableCell) => text.push_str("  "),
            _ => {}
        }
    }
    text.trim().to_owned()
}

/// `text` bounded for a tooltip, which wraps at its own width.
fn bounded(text: &str) -> String {
    let mut lines = text.trim().lines();
    let mut kept: String = lines
        .by_ref()
        .take(TIP_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let mut cut = lines.next().is_some();
    if let Some((end, _)) = kept.char_indices().nth(TIP_CHARS) {
        kept.truncate(end);
        cut = true;
    }
    if cut {
        kept = format!("{}…", kept.trim_end());
    }
    kept
}

/// A one-row step's texts as a reader sees them, without their Markdown.
#[derive(Default)]
pub(super) struct Plain {
    /// The message, or each started agent's prompt in order; empty where it
    /// is not inline.
    said: Vec<String>,
    /// Each answer the step collected, in order.
    answers: Vec<String>,
    /// What the run it waited on returned.
    result: String,
}

impl Plain {
    fn of(reading: &Coordination) -> Self {
        let inline = |text: &Text| text.inline().map(plain).unwrap_or_default();
        Self {
            said: match &reading.kind {
                Kind::Message(message) => vec![inline(&message.body)],
                Kind::Spawn(spawn) => spawn
                    .agents
                    .iter()
                    .map(|agent| inline(&agent.prompt))
                    .collect(),
                Kind::Act(_) => Vec::new(),
            },
            answers: reading
                .outcome
                .messages
                .iter()
                .map(|message| plain(&message.body))
                .collect(),
            result: reading
                .outcome
                .run
                .as_ref()
                .map(|run| plain(&run.result))
                .unwrap_or_default(),
        }
    }

    /// What the step collected, headed by whom it came from, for a tooltip
    /// where the row has no agent to open.
    fn collected(&self, reading: &Coordination) -> Vec<String> {
        reading
            .outcome
            .messages
            .iter()
            .zip(&self.answers)
            .map(|(answer, body)| (format!("From {}", answer.from), body.as_str()))
            .chain(
                reading
                    .outcome
                    .run
                    .as_ref()
                    .map(|_| ("Result".to_owned(), self.result.as_str())),
            )
            .filter(|(_, body)| !body.is_empty())
            .map(|(heading, body)| format!("{heading}:\n{body}"))
            .collect()
    }
}

/// What one coordination row draws.
struct Row {
    glyph: Icon,
    verb: &'static str,
    /// Whom the step reached, whole.
    label: String,
    /// The same as the row draws it: an agent's name, any other name cut to
    /// fit the narrowest pane. It fades where the row runs short.
    shown: String,
    /// What follows the name and tells it apart, the number two agents of
    /// one name go by or a session's ID, which the row keeps whole.
    tag: String,
    /// The start of what was said, or the started agent's type.
    preview: String,
    /// The preview is one term, the agent's type, which the row shows whole
    /// or leaves out: the agent's name and how it stands come first.
    term: bool,
    /// The whole of it, for the tooltip.
    full: String,
    state: RowStatus,
    /// What opening the row does, where this conversation can open the agent.
    open: Option<Action>,
    /// Where it cannot, why: there is none, or there are several.
    several: bool,
    /// The menu's name for copying what the step said.
    copy: &'static str,
}

/// What a row that opens no agent does when pressed, said where the
/// tooltip and a screen reader can tell it apart from one that does.
const OPENS_DETAIL: &str = "No agent to open; pressing shows it in full with its tool call.";
/// The same, for a step that reached several agents, each with its own tab.
const OPENS_DETAIL_OF_SEVERAL: &str =
    "Open each agent from its tab; pressing shows this in full with its tool call.";

impl Row {
    /// What the tooltip and a screen reader give whole: the agent's whole
    /// name where the row cut it, the outcome's reason, what pressing does
    /// where it opens no agent, and then the message or prompt.
    fn described(&self) -> (String, String) {
        // A warning or a failure names itself, as a narrow row keeps only
        // its mark; a plain state is either drawn or the working dot.
        let reason = match (self.state.tone, self.state.detail.as_deref()) {
            (Tone::Plain, detail) => detail.map(str::to_owned),
            // A state with no words of its own, such as an agent that
            // couldn't start, whose verb says so, gives only its reason.
            (_, Some(detail)) if self.state.text.is_empty() => Some(detail.to_owned()),
            (_, Some(detail)) => Some(format!("{}: {detail}", self.state.text)),
            (_, None) => Some(self.state.text.to_string()),
        }
        .filter(|reason| !reason.trim().is_empty());
        let pressing = match (&self.open, self.several) {
            (Some(_), _) => "Opens the agent",
            (None, false) => OPENS_DETAIL,
            (None, true) => OPENS_DETAIL_OF_SEVERAL,
        };
        // The row fades its name where the pane runs short, so the tooltip
        // names it whole.
        let mut tip = vec![if matches!(&self.open, Some(Action::OpenAgent(_))) {
            format!("Open {}", self.label)
        } else {
            format!("{} {}", self.verb, self.label)
        }];
        tip.extend(reason);
        if self.open.is_none() {
            tip.push(pressing.to_owned());
        }
        if !self.full.is_empty() {
            tip.push(bounded(&self.full));
        }
        let name = [
            format!("{} {}", self.verb, self.label),
            self.state.text.to_string(),
            self.state.detail.clone().unwrap_or_default(),
            if matches!(&self.open, Some(Action::OpenAgent(_))) {
                format!("Open {}", self.label)
            } else {
                pressing.to_owned()
            },
            self.preview.clone(),
        ]
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        (tip.join("\n\n"), name)
    }
}

/// A row's trailing status: a word or two, with the whole account of it in
/// the tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RowStatus {
    text: Cow<'static, str>,
    tone: Tone,
    /// Still under way, which a still dot beside its caption marks.
    live: bool,
    detail: Option<String>,
}

impl RowStatus {
    fn with(text: impl Into<Cow<'static, str>>, tone: Tone, live: bool) -> Self {
        Self {
            text: text.into(),
            tone,
            live,
            detail: None,
        }
    }
    fn plain(text: impl Into<Cow<'static, str>>) -> Self {
        Self::with(text, Tone::Plain, false)
    }
    fn live(text: impl Into<Cow<'static, str>>) -> Self {
        Self::with(text, Tone::Plain, true)
    }
    fn warning(text: impl Into<Cow<'static, str>>) -> Self {
        Self::with(text, Tone::Warning, false)
    }
    fn error(text: impl Into<Cow<'static, str>>) -> Self {
        Self::with(text, Tone::Error, false)
    }
    /// The state in words with its reason, as a tooltip lists it.
    fn reason(&self) -> String {
        match self
            .detail
            .as_deref()
            .filter(|detail| !detail.trim().is_empty())
        {
            Some(detail) if self.text.is_empty() => detail.to_owned(),
            Some(detail) => format!("{}: {detail}", self.text),
            None => self.text.to_string(),
        }
    }
}

/// How a run stands, as a row's status, naming the agent that runs it in
/// the tooltip.
fn run_status(run: &Run) -> RowStatus {
    let Some(state) = run.state else {
        return RowStatus {
            detail: (!run.agent.is_empty()).then(|| format!("As agent {}", run.agent)),
            ..RowStatus::plain("Launch accepted")
        };
    };
    let mut status = match state {
        State::Failed | State::Interrupted => RowStatus::error(state_phrase(state)),
        State::Cancelled => RowStatus::warning(state_phrase(state)),
        State::Accepted | State::Running | State::Waiting => RowStatus::live(state_phrase(state)),
        State::Succeeded => RowStatus::plain(state_phrase(state)),
    };
    let mut detail = Vec::new();
    if run.timed_out {
        detail.push("The wait timed out".to_owned());
    }
    if !run.agent.is_empty() {
        detail.push(format!("As agent {}", run.agent));
    }
    status.detail = (!detail.is_empty()).then(|| detail.join(". "));
    status
}

/// A coordination row's context menu entry, for the item `id`.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RowCommand<'a> {
    target: Target,
    id: &'a str,
    copy: bool,
}

impl widget::menu::Action for RowCommand<'_> {
    type Message = Message;

    fn message(&self) -> Message {
        let id = self.id.to_owned();
        action(if self.copy {
            Action::Copy(self.target, id, CopyPart::Text)
        } else {
            Action::Toggle(self.target, id)
        })
    }
}

/// A folded run of messages' context menu entry: copy the words its first
/// message shares with the rest, or show or hide the group `key`.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RunCommand<'a> {
    target: Target,
    key: &'a str,
    first: &'a str,
    copy: bool,
}

impl widget::menu::Action for RunCommand<'_> {
    type Message = Message;

    fn message(&self) -> Message {
        action(if self.copy {
            Action::Copy(self.target, self.first.to_owned(), CopyPart::Text)
        } else {
            Action::ToggleTurn(self.target, self.key.to_owned())
        })
    }
}

/// A glyph in the step rows' muted tone, at `size`: a row's lead at
/// [`design::LEADING_ICON_SIZE`], as a step row's, and its trailing arrow or a
/// caption's mark at [`design::COMPACT_ICON_SIZE`].
fn muted_glyph(icon: Icon, size: u16) -> Element<'static, Message> {
    crate::ui_controls::passive_icon(icon.handle(), size)
}

/// The transcript's own background, which a row's open detail sits on, so
/// a fold fades into it rather than into a card that is no longer there.
fn transcript_surface(theme: &cosmic::Theme) -> iced::Color {
    theme.current_container().base.into()
}

/// How a row's status reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Plain,
    /// It went, but not all the way.
    Warning,
    /// It did not go.
    Error,
}

/// A row's words after its glyph, and what of them its state gives way to.
///
/// The words are the verb, the name with the `tag` that tells it apart kept
/// whole after it, and the preview. The name takes its room before the
/// preview has any, and a preview that is one term, an agent's type, is left
/// out whole rather than cut, so the name fades only once nothing follows it.
/// What must not be cut for the state's sake, the second element, is the
/// verb, as much of the name as [`LABEL_CHARS`] allows and the tag, measured
/// and never drawn: past that, a long name fades rather than hide the state.
fn row_words(
    verb: &'static str,
    name: String,
    tag: String,
    preview: String,
    term: bool,
) -> (Element<'static, Message>, Element<'static, Message>) {
    let measured = Runtime::short_description(&name, LABEL_CHARS);
    let title = crate::ui_controls::FadeText::transcript(name).font(design::transcript_name_font());
    let tagged =
        |tag: String| design::transcript_name(tag).wrapping(iced::widget::text::Wrapping::None);
    let title: Element<'static, Message> = if tag.is_empty() {
        title.into()
    } else {
        crate::ui_controls::trailing(title, tagged(tag.clone()), 0.0)
    };
    let preview = crate::ui_controls::FadeText::transcript(preview)
        .class(cosmic::theme::Text::Custom(design::muted_text))
        .width(Length::Fill);
    let words = widget::row::with_children(vec![
        design::transcript(verb)
            .class(cosmic::theme::Text::Custom(design::muted_text))
            .wrapping(iced::widget::text::Wrapping::None)
            .into(),
        title,
        if term { preview.whole() } else { preview }.into(),
    ])
    .spacing(step::GAP_S)
    .align_y(Alignment::Center)
    .width(Length::Fill);
    let kept = widget::row::with_children(vec![
        design::transcript(verb)
            .wrapping(iced::widget::text::Wrapping::None)
            .into(),
        widget::row::with_children(vec![
            design::transcript_name(measured)
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
            tagged(tag).into(),
        ])
        .into(),
    ])
    .spacing(step::GAP_S);
    (words.into(), kept.into())
}

/// A row's glyph, its words and its status, on the step list's grid: the
/// glyph in the error colour where it failed, and the status kept whole
/// while the words fade, down to what of them must stay.
fn row_content(
    glyph: Icon,
    (words, kept): (Element<'static, Message>, Element<'static, Message>),
    state: &RowStatus,
) -> Element<'static, Message> {
    let lead: Element<'static, Message> = if state.tone == Tone::Error {
        widget::icon(Icon::Error.handle())
            .size(design::LEADING_ICON_SIZE)
            .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(|theme| {
                cosmic::iced::widget::svg::Style {
                    color: Some(design::destructive_color(theme)),
                }
            })))
            .into()
    } else {
        muted_glyph(glyph, design::LEADING_ICON_SIZE)
    };
    // Work under way is marked by a still dot beside its caption; the
    // transcript's one activity indicator says that something moves.
    let live = state.live && matches!(state.tone, Tone::Plain);
    let mut status_cells = Vec::new();
    if live {
        status_cells.push(disclosure::state_dot(design::accent_color));
    }
    // A caption naming what an agent waits on is held to the bound its
    // tab's caption is, so the row says what the tab says.
    let status = crate::ui_controls::FadeText::transcript(state.text.clone()).limit(STATUS_CHARS);
    let status = match state.tone {
        Tone::Plain => status.class(cosmic::theme::Text::Custom(design::muted_text)),
        Tone::Warning => status.class(cosmic::theme::Text::Custom(design::warning_text)),
        Tone::Error => status.class(cosmic::theme::Text::Custom(design::destructive_text)),
    };
    if !state.text.is_empty() {
        status_cells.push(status.into());
    }
    // Where the agent's name and its state no longer both fit, the state
    // keeps only its mark: the dot of work under way, or one in the
    // warning colour. A failure keeps the row's leading glyph in the
    // error colour, and a settled state is told by the absence of either.
    // A warning's words stay in the row's tooltip, and every state's in
    // its accessible name.
    let compact = match state.tone {
        Tone::Plain if live => Some(disclosure::state_dot(design::accent_color)),
        Tone::Warning => Some(disclosure::state_dot(design::warning_color)),
        Tone::Plain | Tone::Error => None,
    };
    let content = crate::ui_controls::yielding(
        words,
        kept,
        widget::row::with_children(status_cells)
            .spacing(step::GAP_S)
            .align_y(Alignment::Center),
        compact,
        step::GAP_S,
    );
    widget::row::with_children(vec![lead, content])
        .spacing(step::GAP_S)
        .align_y(Alignment::Center)
        .width(Length::Fill)
        .into()
}

/// A tracked task's state is independent of the tool that launched it.
#[derive(Debug, PartialEq)]
enum BackgroundState<'a> {
    Working,
    Waiting,
    /// Alive, with its result so far delivered: nothing is asked of anyone.
    /// It holds its agent's caption where that names what it waits on.
    OnBackgroundWork(Option<&'a str>),
    Failed,
    Stopped,
    Denied,
    Ended,
    Finished,
}

impl BackgroundState<'_> {
    fn caption(self) -> Cow<'static, str> {
        Cow::Borrowed(match self {
            Self::Working => "Working",
            Self::Waiting => "Waiting",
            Self::OnBackgroundWork(Some(caption)) => return Cow::Owned(caption.to_owned()),
            Self::OnBackgroundWork(None) => "Waiting on background work",
            Self::Failed => "Failed",
            Self::Stopped => "Stopped",
            Self::Denied => "Denied",
            Self::Ended => "Ended",
            Self::Finished => "Finished",
        })
    }

    /// The row's status for this state: a live caption while the agent works,
    /// and a warning while it waits on the person.
    fn status(self) -> RowStatus {
        match self {
            Self::Working => RowStatus::live(self.caption()),
            Self::Waiting => RowStatus::warning(self.caption()),
            Self::Failed => RowStatus::error(self.caption()),
            _ => RowStatus::plain(self.caption()),
        }
    }
}

impl Runtime {
    /// The subagent of this conversation that `address` names: the ID it is
    /// listed under, or the one its provider addresses it by.
    pub(super) fn subagent(&self, address: &str) -> Option<&chat::Subagent> {
        let listed = self
            .agent_addresses
            .get(address)
            .map_or(address, String::as_str);
        self.snapshot
            .subagents
            .iter()
            .find(|agent| agent.id == listed)
    }

    /// A listed agent's full name and distinguishing ordinal. Transcript rows
    /// bound their own visible previews while retaining this accessible name.
    pub(super) fn listed_agent_name(&self, address: &str) -> Option<String> {
        self.subagent(address)
            .map(|agent| self.child_label(agent, usize::MAX).into_owned())
    }

    /// How a row names a listed agent: as its tab does, but with as much of
    /// its name as the row has room for, which comes before the agent's type
    /// or the start of what was said, and apart from the number that tells
    /// it from another of its name, which the row keeps whole.
    fn row_agent_name(&self, agent: &chat::Subagent) -> (String, String) {
        let name = Self::short_description(Self::native_child_name(agent), ROW_NAME_CHARS);
        match self.child_ordinal(agent) {
            1 => (name, String::new()),
            ordinal => (name, format!(" {ordinal}")),
        }
    }

    /// The agents a step names that this conversation lists, each once.
    fn listed_subagents<'a>(
        &'a self,
        coordinated: &'a Coordinated,
    ) -> impl Iterator<Item = &'a chat::Subagent> {
        let mut seen = Vec::new();
        coordinated
            .reading
            .subagents
            .iter()
            .filter_map(|address| self.subagent(address))
            .filter(move |agent| {
                let new = !seen.contains(&agent.id);
                if new {
                    seen.push(agent.id.clone());
                }
                new
            })
    }

    /// A message or a started agent, drawn as one row of the step list: what
    /// was done and to whom, the start of what was said, and how it went.
    /// The row opens the agent; its context menu copies the message and
    /// opens the detail, which `raw` then draws beneath it.
    pub(super) fn coordination_card<'a>(
        &self,
        item: &chat::Item,
        coordinated: &Coordinated,
        raw: Option<Element<'a, Message>>,
    ) -> Element<'a, Message> {
        let (status, output) = reported(item, coordinated);
        let row = match &coordinated.reading.kind {
            Kind::Message(message) => self.message_row(coordinated, message, status, output),
            Kind::Spawn(spawn) => self.spawn_row(coordinated, spawn, status, output),
            Kind::Act(_) => return widget::Space::new().into(),
        };
        // The row keeps its place in the widget tree whether or not its tool
        // call is open, so the row pressed to open it keeps keyboard focus.
        let mut children = vec![self.row_view(item, row)];
        children.extend(raw.map(|raw| {
            widget::container(raw)
                .padding(iced::Padding::from([0.0, step::GAP_S]).bottom(step::GAP_S))
                .width(Length::Fill)
                .into()
        }));
        widget::column::with_children(children)
            .spacing(step::GAP_XS)
            .width(Length::Fill)
            .into()
    }

    fn message_row(
        &self,
        coordinated: &Coordinated,
        message: &Outbound,
        status: ToolStatus,
        output: &str,
    ) -> Row {
        let reading = &coordinated.reading;
        let whole = |id: &str| {
            self.subagent(id)
                .map(|agent| self.child_label(agent, usize::MAX).into_owned())
        };
        let recipient_whole = recipient(message, reading, &whole);
        let listed: Vec<&chat::Subagent> = self.listed_subagents(coordinated).collect();
        let joined = |agent: &chat::Subagent| {
            let (name, tag) = self.row_agent_name(agent);
            name + &tag
        };
        let drawn = recipient(message, reading, &|id| self.subagent(id).map(joined));
        // One listed agent is named apart from its number, as a started one
        // is; a phrase for several fades as one.
        let (shown, tag) = match listed.as_slice() {
            [agent] if drawn.session.is_none() && drawn.name == joined(agent) => {
                self.row_agent_name(agent)
            }
            _ => drawn.shown(),
        };
        let open = match (listed.as_slice(), &recipient_whole.session) {
            ([agent], _) => Some(Action::SelectSubagent(self.target, Some(agent.id.clone()))),
            (_, Some(session)) if recipient_whole.live => {
                Some(Action::OpenSession(session.clone()))
            }
            _ => None,
        };
        let (preview, full) = match &message.body {
            Text::Inline(_) => {
                let body = coordinated.plain.said.first().cloned().unwrap_or_default();
                (one_line_of(&body), body)
            }
            Text::File(path) => {
                let text = format!("The contents of {path}");
                (text.clone(), text)
            }
            _ => {
                let text = "Read from standard input".to_owned();
                (text.clone(), text)
            }
        };
        let state = message_state(reading, message, status, output);
        // What it answered is in the open detail, and in the agent's
        // transcript; where the row cannot open one, the tooltip gives it too.
        let mut full = vec![full];
        if open.is_none() {
            full.extend(coordinated.plain.collected(reading));
        }
        Row {
            glyph: Icon::Send,
            verb: message_verb(message.kind),
            label: recipient_whole.plain(),
            shown,
            tag,
            preview,
            term: false,
            full: full.join("\n\n").trim().to_owned(),
            state,
            several: open.is_none() && listed.len() > 1,
            open,
            // Without the text inline, copying takes the raw call.
            copy: match message.body.inline() {
                Some(_) => "Copy message",
                None => "Copy tool call",
            },
        }
    }

    fn spawn_row(
        &self,
        coordinated: &Coordinated,
        spawn: &Spawn,
        status: ToolStatus,
        output: &str,
    ) -> Row {
        let reading = &coordinated.reading;
        let listed = match reading.subagents.as_slice() {
            [id] => self.subagent(id),
            _ => None,
        };
        // The agent is named as its tab names it: by its name, else its task,
        // else its type, which then follows it where the task would.
        let cli_label = reading.cli.then(|| {
            let agent = spawn.agents.first();
            crate::agent_execution::agent_label_parts(
                agent.map_or("", |agent| agent.name.as_str()),
                agent.and_then(|agent| agent.prompt.inline()),
            )
        });
        let (label, (shown, tag)) = if let Some((label, _)) = &cli_label {
            (label.clone(), (label.clone(), String::new()))
        } else {
            match listed {
                Some(agent) => (
                    self.child_label(agent, usize::MAX).into_owned(),
                    self.row_agent_name(agent),
                ),
                None => {
                    let label = started(spawn);
                    let shown = Self::short_description(&label, LABEL_CHARS);
                    (label, (shown, String::new()))
                }
            }
        };
        let placed = reading
            .cli
            .then_some(reading.outcome.run.as_ref())
            .flatten()
            .and_then(|run| run.placed_in.as_ref());
        let destination = placed.map(|placed| {
            placed.title.as_ref().map_or_else(
                || "in another channel".to_owned(),
                |title| format!("in channel “{title}”"),
            )
        });
        let mut term = false;
        let mut preview = if let Some((_, task)) = &cli_label {
            task.clone().unwrap_or_default()
        } else {
            match spawn.agents.as_slice() {
                [agent] if agent.kind.trim() != label => {
                    term = true;
                    agent.kind.trim().to_owned()
                }
                [_] => String::new(),
                agents => agents
                    .iter()
                    .map(|agent| {
                        started(&Spawn {
                            agents: vec![agent.clone()],
                            ..Spawn::default()
                        })
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            }
        };
        if let Some(destination) = &destination {
            let duplicate_task = placed
                .and_then(|placed| placed.title.as_deref())
                .is_some_and(|title| {
                    spawn
                        .agents
                        .first()
                        .and_then(|agent| agent.prompt.inline())
                        .and_then(|task| task.trim().lines().next())
                        .and_then(crate::names::normalize_card_name)
                        .as_deref()
                        == Some(title)
                });
            term = false;
            preview = if preview.is_empty() || duplicate_task {
                destination.clone()
            } else {
                format!("{destination} · {preview}")
            };
        }
        let observed_model = listed
            .filter(|_| !reading.cli)
            .and_then(|agent| agent.model.as_deref());
        let mut full: Vec<String> = coordinated
            .plain
            .said
            .iter()
            .filter(|prompt| !prompt.is_empty())
            .cloned()
            .collect();
        if let Some(destination) = &destination {
            full.insert(0, destination.clone());
        }
        let details = spawn_details(reading, spawn, observed_model);
        if !details.is_empty() {
            full.push(details);
        }
        let background = self.background_state(coordinated);
        let run = reading.outcome.run.as_ref();
        // A launch refused before its agent did anything, such as one past
        // the provider's limit of agents at once: there is no agent to open,
        // so pressing the row shows why in its tool call. The CLI names the
        // agent it started; a native agent that started has a transcript.
        let unstarted = matches!(status, ToolStatus::Failed | ToolStatus::Denied)
            && if reading.cli {
                run.is_none()
            } else {
                listed.is_some_and(|agent| agent.items.is_empty())
            };
        let mut glyph = Icon::Agent;
        // Use the observed task state while a native launch tool is still
        // settling, but keep an explicit launch failure visible.
        let state = match background
            .filter(|_| matches!(status, ToolStatus::Running | ToolStatus::Succeeded))
        {
            Some(state) => {
                glyph = match state {
                    BackgroundState::Waiting => Icon::AgentWaiting,
                    BackgroundState::OnBackgroundWork(_) => Icon::AgentBackground,
                    _ => Icon::Agent,
                };
                state.status()
            }
            None => match status {
                ToolStatus::Running if spawn.waits => RowStatus::live("Working"),
                ToolStatus::Running => RowStatus::live("Starting"),
                ToolStatus::Stopped => RowStatus::plain("Stopped"),
                // The verb says it: the agent never started.
                ToolStatus::Failed | ToolStatus::Denied if unstarted => RowStatus {
                    detail: Some(failure(reading, output).to_owned()).filter(|why| !why.is_empty()),
                    ..RowStatus::error("")
                },
                ToolStatus::Failed | ToolStatus::Denied => RowStatus {
                    detail: Some(failure(reading, output).to_owned()).filter(|why| !why.is_empty()),
                    ..RowStatus::error("Failed")
                },
                // Completing the launch command does not complete the
                // separate session: say how it stands, or only that it
                // was accepted.
                _ if reading.cli && !spawn.waits => match run {
                    Some(run) => run_status(run),
                    None => RowStatus::plain("Launch accepted"),
                },
                _ => run.map_or_else(|| RowStatus::plain("Finished"), run_status),
            },
        };
        let open = if reading.cli {
            run.filter(|run| !run.agent.is_empty())
                .map(|run| Action::OpenAgent(run.agent.clone()))
        } else {
            listed
                .filter(|_| !unstarted)
                .map(|agent| Action::SelectSubagent(self.target, Some(agent.id.clone())))
        };
        // What it reported is in the open detail, and in the agent's
        // transcript; where the row cannot open one, the tooltip gives it too.
        // A refused launch's report is only the reason it already gives.
        if open.is_none() && !unstarted {
            full.extend(coordinated.plain.collected(reading));
        }
        Row {
            glyph,
            verb: if unstarted {
                "Couldn't start"
            } else {
                "Started"
            },
            label,
            shown,
            tag,
            preview,
            term,
            full: full.join("\n\n"),
            state,
            several: open.is_none() && self.listed_subagents(coordinated).nth(1).is_some(),
            open,
            copy: if spawn
                .agents
                .iter()
                .any(|agent| agent.prompt.inline().is_some())
            {
                "Copy prompt"
            } else {
                "Copy tool call"
            },
        }
    }

    /// One coordination row, on the step list's grid.
    fn row_view<'a>(&self, item: &chat::Item, row: Row) -> Element<'a, Message> {
        let (tip, name) = row.described();
        let words = row_words(
            row.verb,
            row.shown.clone(),
            row.tag.clone(),
            row.preview.clone(),
            row.term,
        );
        let content = row_content(row.glyph, words, &row.state);
        let open_tool = self.item_expanded(&item.id);
        // The detail holds what was said and reported in full, then the
        // command or tool call that carried it.
        let source = if open_tool {
            "Hide details"
        } else {
            "Show details"
        };
        let (target, id, copy) = (self.target, item.id.clone(), row.copy);
        let menu = move || {
            let command = |copy| RowCommand {
                target,
                id: &id,
                copy,
            };
            widget::menu::items(
                &HashMap::new(),
                vec![
                    widget::menu::Item::Button(copy, None, command(true)),
                    widget::menu::Item::Button(source, None, command(false)),
                ],
            )
        };
        let tipped = |content: Element<'static, Message>| {
            if tip.is_empty() {
                content
            } else {
                crate::interaction::tooltip(content, tip.clone())
            }
        };
        // Every row keeps room for the open arrow at its trailing edge, so
        // the statuses end in line whether or not a row opens.
        let padding = iced::Padding {
            top: step::GAP_XS,
            bottom: step::GAP_XS,
            left: step::GAP_S,
            right: step::GAP_S + ARROW_ROOM,
        };
        // A row that cannot open an agent opens its tool call instead, so the
        // keyboard reaches it, its menu and its outcome all the same.
        let opens = row.open.is_some();
        let pressed = row
            .open
            .unwrap_or_else(|| Action::Toggle(self.target, item.id.clone()));
        let mut button = widget::button::custom(content)
            .name(name)
            .class(crate::ui_controls::control_style(false, None))
            .padding(padding)
            .width(Length::Fill)
            .on_press(action(pressed));
        if !opens {
            // As a step row says whether pressing it shows or hides its detail.
            button = button.description(if open_tool {
                "Collapse details"
            } else {
                "Expand details"
            });
        }
        let button = crate::interaction::a11y_target(button);
        let row: Element<'static, Message> = if opens {
            // The way in, drawn inside the row's highlight where the
            // pointer is. It only marks the row as one that opens: the row
            // is the control, so a click on the arrow is a click on the
            // row, and keyboard traversal and screen readers meet it once.
            let arrow = widget::container(muted_glyph(Icon::Open, design::COMPACT_ICON_SIZE))
                .padding(iced::Padding {
                    right: step::GAP_S,
                    ..iced::Padding::ZERO
                })
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::alignment::Horizontal::Right)
                .align_y(iced::alignment::Vertical::Center);
            hover::over(tipped(button), arrow)
        } else {
            tipped(button)
        };
        crate::interaction::context_menu_lazy(row, menu)
    }

    /// The row a group of messages folds into: how many went to whom, the
    /// words they share or whom they went to, and how they stand, drawn as
    /// each message's own row is with the group's disclosure arrow in place
    /// of the way into an agent. `None` while a step is not yet read as a
    /// message.
    pub(super) fn message_run_row(
        &self,
        turn: &turn::Turn,
        open: bool,
        toggle: Action,
    ) -> Option<Element<'static, Message>> {
        let cache = self.presentation_cache();
        let items = self.step_items();
        let sent = turn
            .steps
            .iter()
            .map(|placed| {
                let item = items.get(placed.index)?;
                Some((&**item, cache.get(&item.id)?.coordination.as_deref()?))
            })
            .collect::<Option<Vec<_>>>()?;
        let run = MessageRun::of(sent, &|id| self.listed_agent_name(id))?;
        let words = row_words(
            run.verb,
            run.count.clone(),
            String::new(),
            run.preview.clone(),
            false,
        );
        let content = row_content(Icon::Send, words, &run.state);
        let arrow = crate::ui_controls::passive_icon(
            if open {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            },
            design::COMPACT_ICON_SIZE,
        );
        let line = widget::row::with_children(vec![content, arrow])
            .spacing(step::GAP_S)
            .align_y(Alignment::Center)
            .width(Length::Fill);
        let button = crate::interaction::a11y_target(
            widget::button::custom(line)
                .name(run.line())
                .description(if open {
                    "Collapse details"
                } else {
                    "Expand details"
                })
                .class(crate::ui_controls::control_style(false, None))
                .width(Length::Fill)
                .padding([step::GAP_XS as u16, step::GAP_S as u16])
                .on_press(action(toggle)),
        );
        let row = crate::interaction::tooltip(button, run.tip());
        // As each message's row does: copy the words they share, and show
        // or hide the messages.
        let (target, key, first, shared) =
            (self.target, turn.key.clone(), run.first, run.full.is_some());
        let shows = if open {
            "Hide messages"
        } else {
            "Show messages"
        };
        let menu = move || {
            let command = |copy| RunCommand {
                target,
                key: &key,
                first: &first,
                copy,
            };
            let mut entries = Vec::new();
            if shared {
                entries.push(widget::menu::Item::Button(
                    "Copy message",
                    None,
                    command(true),
                ));
            }
            entries.push(widget::menu::Item::Button(shows, None, command(false)));
            widget::menu::items(&HashMap::new(), entries)
        };
        Some(crate::interaction::context_menu_lazy(row, menu))
    }

    /// How the agents a step left running in the background are doing now,
    /// for as long as this conversation tracks them.
    fn background_state(&self, coordinated: &Coordinated) -> Option<BackgroundState<'_>> {
        let Kind::Spawn(spawn) = &coordinated.reading.kind else {
            return None;
        };
        if spawn.waits || coordinated.reading.cli {
            return None;
        }
        let agents: Vec<&chat::Subagent> = self.listed_subagents(coordinated).collect();
        let any = |wanted: ToolStatus| agents.iter().any(|agent| agent.status == wanted);
        if agents.is_empty() {
            None
        } else if agents
            .iter()
            .any(|agent| agent.status == ToolStatus::Running && agent.waiting)
        {
            Some(BackgroundState::Waiting)
        } else if any(ToolStatus::Running) {
            Some(BackgroundState::Working)
        } else if any(ToolStatus::Waiting) {
            // One agent's work can be named; several agents' are their own.
            let mut waiting = agents
                .iter()
                .filter(|agent| agent.status == ToolStatus::Waiting);
            let work = match (waiting.next(), waiting.next()) {
                (Some(agent), None) => self.agent_work.get(&agent.id),
                _ => None,
            };
            Some(BackgroundState::OnBackgroundWork(work))
        } else if any(ToolStatus::Failed) {
            Some(BackgroundState::Failed)
        } else if any(ToolStatus::Stopped) {
            Some(BackgroundState::Stopped)
        } else if any(ToolStatus::Denied) {
            // As its tab and report say it.
            Some(BackgroundState::Denied)
        } else if any(ToolStatus::Ended) {
            Some(BackgroundState::Ended)
        } else {
            Some(BackgroundState::Finished)
        }
    }

    /// What a one-row step's open detail draws above its tool call: the
    /// message or prompts in full, formatted, and what came back. A refused
    /// message has no transcript to read it in, and an answer to a question
    /// is its outcome, so neither is left to the tool call's raw text.
    pub(super) fn row_detail(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        coordinated: &Coordinated,
        settings: markdown::Settings,
    ) -> Vec<Element<'static, Message>> {
        let mut rows = Vec::new();
        match &coordinated.reading.kind {
            Kind::Message(message) => rows.extend(self.text_block(
                item,
                cached,
                "body",
                &message.body,
                coordinated.prompts.first().and_then(Option::as_ref),
                "Show full message",
                settings,
            )),
            Kind::Spawn(spawn) => {
                if let Some(brief) = &coordinated.brief {
                    rows.push(self.folded_markdown(
                        item,
                        cached,
                        "brief",
                        brief,
                        "Show shared brief",
                        settings,
                        transcript_surface,
                    ));
                }
                let several = spawn.agents.len() > 1;
                for (index, (agent, prompt)) in
                    spawn.agents.iter().zip(&coordinated.prompts).enumerate()
                {
                    if several {
                        let title = [&agent.name, &agent.description, &agent.kind]
                            .into_iter()
                            .map(|part| part.trim())
                            .filter(|part| !part.is_empty())
                            .collect::<Vec<_>>()
                            .join(" · ");
                        if !title.is_empty() {
                            rows.push(design::caption(title).into());
                        }
                    }
                    rows.extend(self.text_block(
                        item,
                        cached,
                        &format!("prompt-{index}"),
                        &agent.prompt,
                        prompt.as_ref(),
                        "Show full prompt",
                        settings,
                    ));
                }
            }
            Kind::Act(_) => {}
        }
        rows.extend(self.arrived(item, cached, coordinated, "From", settings));
        if let Some(run) = &coordinated.reading.outcome.run
            && !run.result.is_empty()
            && let Some(result) = coordinated.answers.last()
        {
            rows.push(self.answer_block(
                item,
                cached,
                "result",
                "Result".to_owned(),
                result,
                settings,
            ));
        }
        rows
    }

    /// A message's or a prompt's text, folded to its opening when long.
    /// `None` for an empty one, as Codex records a call it replays.
    fn text_block(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        part: &str,
        text: &Text,
        content: Option<&markdown::Content>,
        unfold: &'static str,
        settings: markdown::Settings,
    ) -> Option<Element<'static, Message>> {
        Some(match (text, content) {
            (Text::Inline(text), _) if text.trim().is_empty() => return None,
            (Text::Inline(_), Some(content)) => self.folded_markdown(
                item,
                cached,
                part,
                content,
                unfold,
                settings,
                transcript_surface,
            ),
            (Text::File(path), _) => design::caption(format!("The contents of {path}")).into(),
            _ => design::caption("Read from standard input, which the transcript does not show")
                .into(),
        })
    }

    /// Markdown folded to its opening until the reader opens it. `part` keeps
    /// each fold of one card apart.
    fn folded_markdown(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        part: &str,
        content: &markdown::Content,
        unfold: &'static str,
        settings: markdown::Settings,
        surface: fn(&cosmic::Theme) -> iced::Color,
    ) -> Element<'static, Message> {
        let key = format!("{}#{part}", item.id);
        let open = self.part_unfolded(&key);
        let label = if open { "Show less" } else { unfold }.to_owned();
        let toggle = disclosure::row(
            label.clone(),
            label,
            Icon::Chat,
            None,
            None,
            Some(open),
            Some(Action::Unfold(self.target, key)),
        );
        fold::fold(
            prose::content_view(cached, content, settings, self.target, self.find.as_deref()),
            toggle,
            !open,
            view::DELIVERY_FOLDED_HEIGHT,
            step::GAP_S,
            surface,
        )
    }

    /// Each message the step printed as having arrived, captioned with
    /// `heading` and its sender.
    pub(super) fn arrived(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        coordinated: &Coordinated,
        heading: &str,
        settings: markdown::Settings,
    ) -> Vec<Element<'static, Message>> {
        coordinated
            .reading
            .outcome
            .messages
            .iter()
            .zip(&coordinated.answers)
            .enumerate()
            .map(|(index, (message, content))| {
                self.answer_block(
                    item,
                    cached,
                    &format!("answer-{index}"),
                    format!("{heading} {}", message.from),
                    content,
                    settings,
                )
            })
            .collect()
    }

    /// An answer or a result, set under its own caption. `part` keeps its
    /// fold apart from the card's others.
    fn answer_block(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        part: &str,
        caption: String,
        content: &markdown::Content,
        settings: markdown::Settings,
    ) -> Element<'static, Message> {
        widget::container(
            widget::column::with_children(vec![
                widget::row::with_children(vec![
                    muted_glyph(Icon::Chat, design::COMPACT_ICON_SIZE),
                    design::caption(caption).into(),
                ])
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center)
                .into(),
                self.folded_markdown(
                    item,
                    cached,
                    part,
                    content,
                    "Show more",
                    settings,
                    design::sunken,
                ),
            ])
            .spacing(step::GAP_XS),
        )
        .class(design::surface(
            Some(design::sunken),
            [design::RADIUS_M; 4],
            false,
        ))
        .padding([step::GAP_S as u16, step::GAP_M as u16])
        .width(Length::Fill)
        .into()
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn cli_spawn_preview_names_the_destination_without_repeating_the_task() {
        let (runtime, _handle) = super::super::view::prepared_runtime_for_test(&[]);
        let name = "cross-project-reviewer-with-a-distinctive-name";
        // Longer than the task preview, within the channel title's limit.
        let long_task = "Review the deployment carefully. ".repeat(7);
        let long_destination = format!("in channel “{}”", long_task.trim());
        let oversized_task = "x".repeat(TIP_CHARS + 100);
        let oversized_preview = format!("in channel “Release” · {}…", "x".repeat(160));
        for (placed, title, task, expected) in [
            (
                true,
                Some("Release"),
                "Review deployment",
                "in channel “Release” · Review deployment",
            ),
            (
                true,
                Some("Review the cards"),
                "Review the cards",
                "in channel “Review the cards”",
            ),
            (
                true,
                Some("Review the cards"),
                "Review  the cards",
                "in channel “Review the cards”",
            ),
            (
                true,
                Some("Review the cards"),
                "\n Review the cards \nKeep the details",
                "in channel “Review the cards”",
            ),
            (true, Some("Release"), "", "in channel “Release”"),
            (
                true,
                None,
                "Review deployment",
                "in another channel · Review deployment",
            ),
            (false, None, "Review deployment", "Review deployment"),
            (
                true,
                Some(long_task.trim()),
                long_task.as_str(),
                long_destination.as_str(),
            ),
            (
                true,
                Some("Release"),
                oversized_task.as_str(),
                oversized_preview.as_str(),
            ),
        ] {
            let spawn = Spawn {
                agents: vec![crate::chat::coordination::Delegate {
                    name: name.into(),
                    kind: String::new(),
                    description: String::new(),
                    prompt: Text::Inline(task.into()),
                }],
                ..Default::default()
            };
            let reading = Coordinated::new(Coordination {
                kind: Kind::Spawn(spawn.clone()),
                outcome: crate::chat::coordination::Outcome {
                    run: Some(Run {
                        agent: "1a2b3c4d".into(),
                        placed_in: placed.then(|| crate::control::PlacedIn {
                            channel: crate::model::ChannelId::new(),
                            title: title.map(str::to_owned),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                subagents: Vec::new(),
                cli: true,
            });
            let row = runtime.spawn_row(&reading, &spawn, ToolStatus::Succeeded, "");
            assert_eq!(row.verb, "Started");
            assert_eq!(row.shown, name, "the name stays whole in either layout");
            assert_eq!(row.preview, expected);
            assert_eq!(row.state.text, "Launch accepted");
            let (tip, accessible) = row.described();
            assert!(accessible.contains(expected));
            if placed {
                let destination = title.map_or_else(
                    || "in another channel".to_owned(),
                    |title| format!("in channel “{title}”"),
                );
                assert!(tip.contains(&destination));
            }
        }
    }

    fn running_command(id: &str, command: &str) -> chat::Item {
        chat::Item {
            id: id.into(),
            role: chat::Role::Assistant,
            content: ItemContent::Command {
                label: "Run the check".into(),
                command: command.into(),
                working_directory: String::new(),
                action: chat::CommandAction::Run,
                status: ToolStatus::Running,
                exit_code: None,
                duration_ms: None,
                output: String::new(),
            },
            started_at: None,
            at: None,
        }
    }

    /// A background agent's row says what it waits on in the words its tab
    /// uses, and shows its type whole or not at all.
    #[test]
    fn a_waiting_agents_row_names_what_it_waits_on_and_its_type_is_a_whole_term() {
        let (mut runtime, _handle) = super::super::view::prepared_runtime_for_test(&[]);
        let reading = claude_step(
            "Agent",
            serde_json::json!({
                "description": "Review restart safety",
                "subagent_type": "general-purpose",
                "prompt": "Review restart safety.",
                "run_in_background": true
            }),
            "Async agent launched successfully.",
        );
        let Kind::Spawn(spawn) = reading.reading.kind.clone() else {
            panic!("an Agent call starts an agent");
        };
        std::sync::Arc::make_mut(&mut runtime.snapshot).subagents = vec![chat::Subagent {
            id: "toolu_step".into(),
            kind: String::new(),
            description: "Review restart safety".into(),
            status: ToolStatus::Waiting,
            model: None,
            waiting: false,
            detail: String::new(),
            background: true,
            items: vec![running_command("check", "scripts/restart-check\n--verbose").into()],
        }];
        // Each change arrives as a publication, which names what each agent
        // waits on once; drawing only looks it up.
        let observe = |runtime: &mut Runtime| {
            let mut snapshot = (*runtime.snapshot).clone();
            snapshot.generation += 1;
            runtime.install_snapshot(std::sync::Arc::new(snapshot), None);
        };
        observe(&mut runtime);
        let row =
            |runtime: &Runtime| runtime.spawn_row(&reading, &spawn, ToolStatus::Succeeded, "");
        let started = row(&runtime);
        assert_eq!(started.shown, "Review restart safety");
        assert_eq!(started.preview, "general-purpose");
        assert!(started.term, "the type is drawn whole or left out");
        assert_eq!(started.state.text, "Waiting on scripts/restart-check");
        let agent = &runtime.snapshot.subagents[0];
        assert_eq!(
            runtime.native_child_status(agent),
            started.state.text,
            "the row and the agent's tab say the same"
        );
        // The same publication installed again is not scanned again, though
        // what is shown of it is its prose joined, not the publication.
        let mut again = (*runtime.snapshot).clone();
        again.generation += 1;
        let message = "joined ".repeat(chat::TRANSCRIPT_PART_BYTES / 3);
        let cut = super::super::logical_prose::parts("answer", &message, 0..usize::MAX);
        assert!(cut.len() > 1, "the message is held in parts");
        let source = chat::TextCopySource {
            cache: uuid::Uuid::from_u128(7),
            scope: chat::TextCopyScope::Conversation,
            token: 3,
            revision: 1,
            bytes: message.len() as u64,
        };
        again.text_copy_sources.conversation = vec![Some(source); cut.len()];
        again.items = cut;
        let again = std::sync::Arc::new(again);
        runtime.install_snapshot(again.clone(), None);
        assert!(
            !std::sync::Arc::ptr_eq(&runtime.snapshot, &again),
            "the shown publication is the joined one"
        );
        runtime.agent_work = Default::default();
        runtime.install_snapshot(again, None);
        assert_eq!(runtime.agent_work.get("toolu_step"), None);
        // Several commands are counted, and work it never showed is unnamed.
        std::sync::Arc::make_mut(&mut runtime.snapshot).subagents[0]
            .items
            .push(running_command("soak", "scripts/soak").into());
        observe(&mut runtime);
        assert_eq!(row(&runtime).state.text, "Waiting on 2 commands");
        // A long command is cut once, where the tab cuts it, and the row's
        // bound leaves that cut whole.
        std::sync::Arc::make_mut(&mut runtime.snapshot).subagents[0].items =
            vec![running_command("long", "scripts/restart-check-every-release-path").into()];
        observe(&mut runtime);
        let long = row(&runtime).state.text;
        assert_eq!(long, "Waiting on scripts/restart-check-ev…");
        assert_eq!(long.chars().count(), STATUS_CHARS);
        std::sync::Arc::make_mut(&mut runtime.snapshot).subagents[0]
            .items
            .clear();
        observe(&mut runtime);
        assert_eq!(row(&runtime).state.text, "Waiting on background work");
        // Settled, it names no work.
        std::sync::Arc::make_mut(&mut runtime.snapshot).subagents[0].status = ToolStatus::Succeeded;
        observe(&mut runtime);
        assert_eq!(row(&runtime).state.text, "Finished");
    }

    /// A row for several agents names what one waits on only when it is the
    /// one waiting; the work of several is theirs to say on their own tabs.
    #[test]
    fn a_row_for_several_agents_names_work_only_when_one_waits() {
        let (mut runtime, _handle) = super::super::view::prepared_runtime_for_test(&[]);
        let agent = |id: &str, status: ToolStatus, command: &str| chat::Subagent {
            id: id.into(),
            kind: id.into(),
            description: String::new(),
            status,
            model: None,
            waiting: false,
            detail: String::new(),
            background: true,
            items: vec![running_command(&format!("{id}-check"), command).into()],
        };
        let delegate = |name: &str| crate::chat::coordination::Delegate {
            name: name.into(),
            kind: String::new(),
            description: String::new(),
            prompt: Text::Inline(String::new()),
        };
        let reading = Coordinated::new(Coordination {
            kind: Kind::Spawn(Spawn {
                agents: vec![delegate("docs"), delegate("tests")],
                ..Default::default()
            }),
            outcome: Default::default(),
            subagents: vec!["docs".into(), "tests".into()],
            cli: false,
        });
        let mut state = |docs: ToolStatus, tests: ToolStatus| {
            std::sync::Arc::make_mut(&mut runtime.snapshot).subagents = vec![
                agent("docs", docs, "scripts/build-docs"),
                agent("tests", tests, "scripts/test"),
            ];
            runtime.agent_work.observe(&runtime.snapshot.subagents);
            runtime
                .background_state(&reading)
                .map(|state| state.caption().into_owned())
        };
        assert_eq!(
            state(ToolStatus::Waiting, ToolStatus::Succeeded).as_deref(),
            Some("Waiting on scripts/build-docs")
        );
        assert_eq!(
            state(ToolStatus::Waiting, ToolStatus::Waiting).as_deref(),
            Some("Waiting on background work")
        );
        // A refused agent reads as its tab says, not as a stopped one.
        assert_eq!(
            state(ToolStatus::Denied, ToolStatus::Succeeded).as_deref(),
            Some("Denied")
        );
        assert_eq!(
            state(ToolStatus::Denied, ToolStatus::Stopped).as_deref(),
            Some("Stopped")
        );
    }

    fn claude_step(name: &str, input: serde_json::Value, output: &str) -> Coordinated {
        let item = chat::Item {
            id: "toolu_step".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Tool {
                name: name.into(),
                input: input.to_string(),
                output: output.into(),
                status: ToolStatus::Succeeded,
            },
            started_at: None,
            at: None,
        };
        Coordinated::new(chat::coordination::recognize(&item, Provider::Claude).unwrap())
    }

    /// An agent Claude refused to start, past its limit of agents at once,
    /// says so where it was asked for, and pressing it shows why; one that
    /// worked before it failed is still an agent to open.
    #[test]
    fn a_refused_launch_reads_as_one_that_never_started() {
        let refusal =
            "Concurrent subagent limit reached. You can run 20 subagents at once. Do not retry.";
        let (mut runtime, _handle) = super::super::view::prepared_runtime_for_test(&[]);
        let step = claude_step(
            "Agent",
            serde_json::json!({"description": "Review the change", "prompt": "Review the change on this branch.", "subagent_type": "general-purpose"}),
            refusal,
        );
        let Kind::Spawn(spawn) = &step.reading.kind else {
            panic!("an Agent call starts an agent");
        };
        for worked in [false, true] {
            let items = if worked {
                vec![std::sync::Arc::new(chat::Item {
                    id: "child".into(),
                    role: chat::Role::Assistant,
                    content: ItemContent::Text {
                        text: "Reading the diff.".into(),
                        thinking: false,
                        phase: chat::Phase::Final,
                    },
                    started_at: None,
                    at: None,
                })]
            } else {
                Vec::new()
            };
            std::sync::Arc::make_mut(&mut runtime.snapshot).subagents = vec![chat::Subagent {
                id: "toolu_step".into(),
                kind: "general-purpose".into(),
                description: "Review the change".into(),
                status: ToolStatus::Failed,
                model: None,
                waiting: false,
                detail: refusal.into(),
                background: false,
                items,
            }];
            let row = runtime.spawn_row(&step, spawn, ToolStatus::Failed, refusal);
            let (tip, accessible) = row.described();
            assert!(tip.contains(refusal), "{worked}: {tip}");
            assert!(accessible.contains(refusal), "{worked}: {accessible}");
            if worked {
                assert_eq!(row.verb, "Started");
                assert_eq!(row.state.text, "Failed");
                // The tooltip names the agent whole, then how it stands.
                assert!(
                    tip.starts_with(&format!("Started {}\n\nFailed: {refusal}", row.label)),
                    "{tip}"
                );
                assert!(row.open.is_some(), "its transcript opens");
            } else {
                assert_eq!(row.verb, "Couldn't start");
                assert_eq!(row.state.text, "", "the verb says it failed");
                assert!(row.open.is_none(), "there is no agent to open");
                assert!(accessible.starts_with("Couldn't start "), "{accessible}");
                assert!(tip.contains(OPENS_DETAIL), "{tip}");
                assert_eq!(tip.matches(refusal).count(), 1, "the reason once: {tip}");
                // A state with no words gives its bare reason, not ": reason",
                // after what could not start.
                assert!(
                    tip.starts_with(&format!("Couldn't start {}\n\n{refusal}", row.label)),
                    "{tip}"
                );
            }
        }
    }

    /// However long an agent's name or a step's text, its row stays one line
    /// in a narrow pane, as tall as a row with short ones. The row's
    /// accessible name keeps the agent whole, and drawing the row leaves the
    /// transcript item as it was.
    #[test]
    fn a_narrow_row_stays_one_line_and_names_its_agent_whole() {
        use iced::advanced::{Layout, layout, widget::Tree};
        use iced_accessibility::accesskit::Role;

        let rows = |name: &str, task: &str, message: &str| {
            let items = [
                (
                    "spawn",
                    "Agent",
                    serde_json::json!({
                        "subagent_type": name,
                        "description": task,
                        "prompt": message
                    }),
                ),
                (
                    "message",
                    "SendMessage",
                    serde_json::json!({
                        "to": "spawn",
                        "message": message,
                        "summary": "Review request"
                    }),
                ),
            ]
            .map(|(id, tool, input)| chat::Item {
                id: id.into(),
                role: chat::Role::Assistant,
                content: ItemContent::Tool {
                    name: tool.into(),
                    input: input.to_string(),
                    output: String::new(),
                    status: ToolStatus::Succeeded,
                },
                started_at: None,
                at: None,
            });
            let (mut runtime, _handle) = super::super::view::prepared_runtime_for_test(&items);
            Arc::make_mut(&mut runtime.snapshot)
                .subagents
                .push(chat::Subagent {
                    id: "spawn".into(),
                    kind: name.into(),
                    description: task.into(),
                    status: ToolStatus::Succeeded,
                    model: None,
                    waiting: false,
                    detail: String::new(),
                    background: false,
                    items: Vec::new(),
                });
            let renderer = crate::app::tests::headless_renderer();
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let cached = &runtime.cache[&item.id];
                    let coordinated = cached.coordination.as_deref().unwrap();
                    let mut element = runtime.coordination_card(item, coordinated, None);
                    let mut tree = Tree::new(element.as_widget());
                    let node = element.as_widget_mut().layout(
                        &mut tree,
                        &renderer,
                        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(200.0, 800.0)),
                    );
                    let accessible = element.as_widget().a11y_nodes(
                        Layout::new(&node),
                        &tree,
                        iced::mouse::Cursor::Unavailable,
                    );
                    let named = accessible
                        .root()
                        .iter()
                        .chain(accessible.children())
                        .filter(|node| node.node().role() == Role::Button)
                        .filter_map(|node| node.node().label().map(str::to_owned))
                        .collect::<Vec<_>>();
                    assert_eq!(runtime.snapshot.items[index].as_ref(), item);
                    (node.size(), named)
                })
                .collect::<Vec<_>>()
        };
        let name = format!("Reviewer {}", "界".repeat(300));
        let task = format!("Verify {}\nRetain the full task", "permissions ".repeat(80));
        let long = rows(&name, &task, &"Review the fixture ".repeat(200));
        let short = rows("Reviewer", "Verify", "Review the fixture");
        for ((long_size, named), (short_size, _)) in long.iter().zip(&short) {
            assert_eq!(long_size.width, 200.0);
            assert_eq!(
                long_size.height, short_size.height,
                "a long name or text must not wrap the row"
            );
            assert!(
                matches!(named.as_slice(), [label] if label.contains(&name)),
                "the row is one control named with its agent whole: {named:?}"
            );
        }
    }

    /// Where a row runs short, its agent's type goes first, whole. The name
    /// keeps its width until nothing follows it, then fades, and the number
    /// that tells two agents of one name apart stays whole after it. What the
    /// row's state gives way to is the verb, the name and the number, whole.
    #[test]
    fn a_rows_name_has_room_before_its_agents_type() {
        use iced::advanced::{layout, widget::Tree};

        let renderer = crate::app::tests::headless_renderer();
        let lay = |element: Element<'static, Message>, width: f32| {
            let mut element = element;
            let mut tree = Tree::new(element.as_widget());
            element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 100.0)),
            )
        };
        let words = |tag: &str, width: f32| {
            let (words, _) = row_words(
                "Started",
                "Review restart safety".into(),
                tag.into(),
                "general-purpose".into(),
                true,
            );
            lay(words, width)
        };
        let parts = |node: &layout::Node| {
            node.children()
                .iter()
                .map(|child| child.size().width)
                .collect::<Vec<_>>()
        };
        let gap = step::GAP_S;
        // With room to spare, each part is laid out at its own width.
        let wide = parts(&words("", 4000.0));
        let (verb, name, kind) = (wide[0], wide[1], wide[2]);
        assert!(verb > 0.0 && name > 0.0 && kind > 0.0, "{wide:?}");
        // Room for the name but not the type: the type has only what is
        // left, which is too little to draw it whole.
        let short = parts(&words("", verb + name + 2.0 * gap + 20.0));
        assert_eq!((short[0], short[1]), (verb, name), "{short:?}");
        assert!(short[2] <= 20.0, "{short:?}");
        // Less room: the type is gone and the name fades.
        let room = verb + gap + name / 2.0;
        let narrow = parts(&words("", room));
        assert_eq!(narrow[0], verb, "{narrow:?}");
        assert!(narrow[1] < name, "the name fades: {narrow:?}");
        // The number stays whole where the name fades before it.
        let number = |width: f32| parts(&words(" 2", width).children()[1]);
        let whole = number(4000.0);
        let cut = number(room);
        assert!(whole[1] > 0.0, "{whole:?}");
        assert_eq!(cut[1], whole[1], "the number keeps its room: {cut:?}");
        assert!(cut[0] < whole[0], "the name fades before it: {cut:?}");
        // The state gives way to the verb, the name and the number whole,
        // however long the type that follows them.
        let (_, kept) = row_words(
            "Started",
            "Review restart safety".into(),
            " 2".into(),
            "general-purpose ".repeat(40),
            false,
        );
        let kept = lay(kept, 4000.0).size().width;
        assert_eq!(kept, verb + gap + whole[0] + whole[1]);
        // A long name is kept only as far as a recipient's would be: past
        // that it fades, and the state keeps its place beside it.
        let long = "Confirm every supported upgrade path still migrates from the oldest release";
        let kept = |name: &str| {
            let (_, kept) = row_words("Started", name.into(), String::new(), String::new(), true);
            lay(kept, 4000.0).size().width
        };
        assert_eq!(
            kept(long),
            kept(&Runtime::short_description(long, LABEL_CHARS))
        );
    }

    /// Claude addresses an agent by the agent ID it gave it, never by the
    /// call that started it: the card names the agent as its tab does, and
    /// one the conversation does not list without showing its ID.
    #[test]
    fn a_message_names_its_recipient_as_the_agent_tab_does() {
        let (mut runtime, _handle) = super::super::view::prepared_runtime_for_test(&[]);
        std::sync::Arc::make_mut(&mut runtime.snapshot).subagents = vec![chat::Subagent {
            id: "toolu_relocate".into(),
            kind: "general-purpose".into(),
            description: "Relocate the repo".into(),
            status: ToolStatus::Running,
            model: None,
            waiting: false,
            detail: String::new(),
            background: true,
            items: Vec::new(),
        }];
        // As the daemon says the provider addresses it.
        runtime.agent_addresses =
            std::sync::Arc::new([("a1892b93e88999a7d".into(), "toolu_relocate".into())].into());
        let listed = |id: &str| runtime.listed_agent_name(id);
        let send = |to: &str, output: &str| {
            claude_step(
                "SendMessage",
                serde_json::json!({"to": to, "message": "The repo moved", "summary": "repo moved"}),
                output,
            )
        };
        let line = |coordinated: &Coordinated| {
            let (verb, label) = coordinated.line(&listed);
            format!("{verb} {label}")
        };
        let matched = send("a1892b93e88999a7d", "");
        assert_eq!(line(&matched), "Message to general-purpose");
        assert_eq!(
            runtime
                .listed_subagents(&matched)
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            ["toolu_relocate"],
            "the card links the transcript of the agent it reached"
        );
        // An agent from an earlier transcript, or one this conversation does
        // not list, is not named by its ID.
        let unmatched = send("a77f00d1e2b3c4d5a", "");
        assert_eq!(line(&unmatched), "Message to an agent");
        assert_eq!(runtime.listed_subagents(&unmatched).count(), 0);
        // A name is the agent's own word for it, until the result pins it.
        assert_eq!(line(&send("release-notes", "")), "Message to release-notes");
        let pinned = send(
            "relocate",
            r#"{"success":true,"message":"Message queued for delivery to relocate at its next tool round.","pin":{"id":"a1892b93e88999a7d","name":"relocate"}}"#,
        );
        assert_eq!(line(&pinned), "Message to general-purpose");
        assert_eq!(line(&send("*", "")), "Message to every teammate");
        assert_eq!(
            line(&send("uds:/run/user/1000/cc-socks/1.sock", "")),
            "Message to another session"
        );
        // A background agent's task is its agent ID; a shell's says nothing.
        let stop = |task: &str| {
            line(&claude_step(
                "TaskStop",
                serde_json::json!({"task_id": task}),
                "",
            ))
        };
        assert_eq!(stop("a1892b93e88999a7d"), "Stopped general-purpose");
        assert_eq!(stop("bq3x9k2f1"), "Stopped a background task");
    }

    #[test]
    fn agents_a_step_cannot_name_are_counted() {
        let reading = |cli| Coordination {
            kind: Kind::Act(Act::Agents),
            outcome: Default::default(),
            subagents: Vec::new(),
            cli,
        };
        let listed = |id: &str| (id == "thread-a").then(|| "reviewer".to_owned());
        let agents = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        let native = reading(false);
        let phrase = |ids: &[&str]| agents_phrase(&native, &agents(ids), &listed);
        assert_eq!(phrase(&["thread-a"]), "reviewer");
        assert_eq!(
            phrase(&["thread-a", "019a4c2e-7f10-7c3e-9b1d-2f5e8a6c4d01"]),
            "reviewer and another agent"
        );
        assert_eq!(
            phrase(&["019a4c2e-7f10", "019a4c2e-7f11", "thread-a"]),
            "reviewer and 2 other agents"
        );
        assert_eq!(phrase(&["019a4c2e-7f10", "019a4c2e-7f11"]), "2 agents");
        // A `cagents` agent goes by the short ID the CLI prints for it.
        assert_eq!(
            agents_phrase(&reading(true), &agents(&["95fcd1b9e0aa"]), &listed),
            "95fcd1b9"
        );
    }

    #[test]
    fn a_message_taken_as_a_run_reads_as_queued_or_delivered_and_an_unknown_end_is_unconfirmed() {
        let word = |arrival| {
            let status = received(arrival);
            (status.text.into_owned(), status.tone == Tone::Warning)
        };
        for waiting in [Arrival::Queued, Arrival::Behind, Arrival::NoSlot] {
            assert_eq!(word(waiting), ("Queued".to_owned(), false));
        }
        for given in [Arrival::Started, Arrival::Ended] {
            assert_eq!(word(given), ("Delivered".to_owned(), false));
        }
        assert_eq!(word(Arrival::Unknown), ("Not confirmed".to_owned(), true));
        assert_eq!(
            received(Arrival::Unknown).detail.as_deref(),
            Some(arrival(Arrival::Unknown))
        );
    }

    #[test]
    fn a_rows_text_reads_as_the_message_without_its_markdown() {
        let message = "The checkout moved, so **a few things** need to follow it:\n\n\
            1. The repository is now at `/srv/repos/t3code`.\n\
            2. Re-run `pnpm install` [there](https://example.com).\n\n\
            - keep `.env.local`\n\n\
            ```sh\ncd /srv/repos/t3code\n```\n\nThen report back.";
        assert_eq!(
            plain(message),
            "The checkout moved, so a few things need to follow it:\n\n\
             1. The repository is now at /srv/repos/t3code.\n\
             2. Re-run pnpm install there.\n\n\
             • keep .env.local\n\n\
             cd /srv/repos/t3code\n\n\
             Then report back."
        );
        assert_eq!(
            one_line_of(&plain(message)),
            "The checkout moved, so a few things need to follow it: 1. The repository is \
             now at /srv/repos/t3code. 2. Re-run pnpm install there. • keep .env.local cd \
             /srv/repos/t3code Then report back."
        );
        // Only what a tooltip can show is read.
        assert!(plain(&"word ".repeat(10_000)).len() <= 4 * TIP_CHARS);
    }

    #[test]
    fn launches_describe_only_what_was_reported() {
        let mut spawn = Spawn::default();
        let mut reading = Coordination {
            kind: Kind::Spawn(spawn.clone()),
            outcome: Default::default(),
            subagents: Vec::new(),
            cli: false,
        };
        assert_eq!(spawn_details(&reading, &spawn, None), "");
        assert_eq!(
            spawn_details(&reading, &spawn, Some("gpt-child-reported")),
            "gpt-child-reported"
        );
        spawn.agents.push(crate::chat::coordination::Delegate {
            name: String::new(),
            kind: "general-purpose".into(),
            description: "Relocate the repo".into(),
            prompt: Text::Inline("Move it".into()),
        });
        assert_eq!(spawn_details(&reading, &spawn, None), "general-purpose");
        spawn.agents.clear();
        reading.cli = true;
        assert_eq!(spawn_details(&reading, &spawn, None), "cagents agent");
        spawn.provider = "codex".into();
        spawn.model = "gpt-reported".into();
        spawn.effort = "low".into();
        let channel = crate::model::ChannelId::new();
        spawn.channel = Some(channel);
        assert_eq!(
            spawn_details(&reading, &spawn, None),
            format!("cagents agent · codex · gpt-reported · low · Channel {channel}")
        );
        spawn.placement = Some(crate::agent_execution::Placement::Worktree {
            repository: "/repo".into(),
            path: "/repo/tree".into(),
        });
        spawn.directory = "/repo".into();
        assert_eq!(
            spawn_details(&reading, &spawn, None),
            "cagents agent · codex · gpt-reported · low · Worktree view /repo/tree · in /repo"
        );
    }

    #[test]
    fn a_rows_tooltip_names_a_cut_agent_whole_and_says_what_a_row_without_one_does() {
        let row = |label: &str, shown: &str, open: Option<Action>, state: RowStatus| Row {
            glyph: Icon::Send,
            verb: "Message to",
            label: label.to_owned(),
            shown: shown.to_owned(),
            tag: String::new(),
            preview: "The repo is now at /srv".to_owned(),
            term: false,
            full: "The repo is now at /srv/repos/t3code.".to_owned(),
            state,
            open,
            several: false,
            copy: "Copy message",
        };
        let long = "Relocate the t3code checkout and every worktree";
        let shown = Runtime::short_description(long, super::super::child_navigation::TAB_CHARS);
        let opens = row(
            long,
            &shown,
            Some(Action::OpenSession("95fcd1b9".into())),
            RowStatus::plain("Queued"),
        );
        let (tip, name) = opens.described();
        assert_eq!(
            tip,
            format!("Message to {long}\n\nThe repo is now at /srv/repos/t3code.")
        );
        assert_eq!(
            name,
            format!("Message to {long} · Queued · Opens the agent · The repo is now at /srv")
        );
        // A short name is named whole too: the row may still fade it where
        // the pane runs short.
        let (tip, _) = row(
            "reviewer",
            "reviewer",
            opens.open.clone(),
            RowStatus::plain("Queued"),
        )
        .described();
        assert_eq!(
            tip,
            "Message to reviewer\n\nThe repo is now at /srv/repos/t3code."
        );

        // A row with no agent to open says so, after the outcome's reason.
        let refused = || RowStatus {
            detail: Some("No agent named 'release-notes' is reachable.".into()),
            ..RowStatus::error("Not sent")
        };
        let (tip, name) = row("release-notes", "release-notes", None, refused()).described();
        assert_eq!(
            tip,
            format!(
                "Message to release-notes\n\n\
                 Not sent: No agent named 'release-notes' is reachable.\n\n{OPENS_DETAIL}\n\n\
                 The repo is now at /srv/repos/t3code."
            )
        );
        assert_eq!(
            name,
            format!(
                "Message to release-notes · Not sent · No agent named 'release-notes' is \
                 reachable. · {OPENS_DETAIL} · The repo is now at /srv"
            )
        );
        // One that reached several agents does not claim there is none.
        let several = Row {
            several: true,
            ..row(
                "Docs, Tests",
                "Docs, Tests",
                None,
                RowStatus::plain("Delivered"),
            )
        };
        assert!(several.described().0.contains(OPENS_DETAIL_OF_SEVERAL));
    }

    /// A row names an agent as its tab does, so two whose tasks a tab cuts
    /// to one text keep the number that tells them apart; a session it
    /// messaged keeps the ID that tells it apart, however long its title.
    #[test]
    fn a_rows_name_keeps_what_tells_its_agent_apart() {
        let (mut runtime, _handle) = super::super::view::prepared_runtime_for_test(&[]);
        let task = "Investigate the flaky test in the parser module";
        let agent = |id: &str| chat::Subagent {
            id: id.into(),
            kind: task.into(),
            description: task.into(),
            status: ToolStatus::Running,
            model: None,
            waiting: false,
            detail: String::new(),
            background: true,
            items: Vec::new(),
        };
        std::sync::Arc::make_mut(&mut runtime.snapshot).subagents =
            vec![agent("toolu_first"), agent("toolu_second")];
        let second = &runtime.snapshot.subagents[1];
        let tab = runtime.child_label(second, super::super::child_navigation::TAB_CHARS);
        assert!(tab.ends_with(" 2"), "{tab}");
        // The row has room for more of the name than the tab, and keeps the
        // number apart from it, whole where the name fades.
        assert_eq!(
            runtime.row_agent_name(second),
            (task.to_owned(), " 2".to_owned())
        );

        let titled = Recipient {
            name: "A session whose title runs on well past a row".into(),
            session: Some("95fcd1b9e3a4".into()),
            live: true,
        };
        let (shown, id) = titled.shown();
        assert_eq!(id, " (95fcd1b9)");
        assert!(
            shown.chars().count() + id.chars().count() <= LABEL_CHARS + 1,
            "{shown}{id}"
        );
        // A phrase the row draws whole.
        let report = Recipient {
            name: "the conversation that started it".into(),
            session: None,
            live: false,
        };
        assert_eq!(report.shown(), (report.name.clone(), String::new()));
    }
}
