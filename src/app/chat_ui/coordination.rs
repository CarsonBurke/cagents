//! Drawing what an agent did to work with other agents.
//!
//! A message one agent sends another, and an agent one starts, is routine
//! activity, so each is one row of the step list: "Message to Relocate the
//! repo", the start of what it said, and how it went. The row opens the
//! agent; its tooltip gives the whole message, and its context menu copies
//! it and shows the command or tool call behind it. What the agent answered
//! is in its transcript. Every other coordinating step stays one line of the
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
    #[cfg(test)]
    pub(super) created_on: std::thread::ThreadId,
    #[cfg(test)]
    disposed: Option<std::sync::mpsc::Sender<std::thread::ThreadId>>,
}
impl Prepared {
    /// The pull requests the texts this card parses link to.
    pub(super) fn links(&self) -> Vec<crate::github_identity::PullRequestKey> {
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

    /// The text of each part the card folds, by the part's name.
    pub(super) fn parts(&self) -> Vec<(String, &str)> {
        let mut parts = Vec::new();
        match &self.reading.kind {
            Kind::Message(message) => {
                parts.extend(message.body.inline().map(|body| ("body".to_owned(), body)))
            }
            Kind::Spawn(spawn) => {
                parts.push(("brief".to_owned(), spawn.brief.as_str()));
                parts.extend(
                    spawn
                        .agents
                        .iter()
                        .enumerate()
                        .filter_map(|(index, agent)| {
                            Some((format!("prompt-{index}"), agent.prompt.inline()?))
                        }),
                );
            }
            Kind::Act(_) => {}
        }
        let outcome = &self.reading.outcome;
        parts.extend(
            outcome
                .messages
                .iter()
                .enumerate()
                .map(|(index, message)| (format!("answer-{index}"), message.body.as_str())),
        );
        parts.extend(
            outcome
                .run
                .as_ref()
                .map(|run| ("result".to_owned(), run.result.as_str())),
        );
        parts
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

/// How many characters of an agent's name a row shows.
const LABEL_CHARS: usize = 40;
/// The room a row keeps at its trailing edge for the open arrow: the glyph
/// and the gap before it.
const ARROW_ROOM: f32 = 14.0 + step::GAP_S;
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

/// What one coordination row draws.
struct Row {
    glyph: Icon,
    verb: &'static str,
    label: String,
    /// The start of what was said, or the started agent's type.
    preview: String,
    /// The whole of it, for the tooltip.
    full: String,
    state: RowStatus,
    /// What opening the row does, where this conversation can open the agent.
    open: Option<Action>,
}

/// A row's trailing status: a word or two, with the whole account of it in
/// the tooltip.
struct RowStatus {
    text: &'static str,
    tone: Tone,
    /// Still under way, which a spinner beside it says.
    live: bool,
    detail: Option<String>,
}

impl RowStatus {
    fn with(text: &'static str, tone: Tone, live: bool) -> Self {
        Self {
            text,
            tone,
            live,
            detail: None,
        }
    }
    fn plain(text: &'static str) -> Self {
        Self::with(text, Tone::Plain, false)
    }
    fn live(text: &'static str) -> Self {
        Self::with(text, Tone::Plain, true)
    }
    fn warning(text: &'static str) -> Self {
        Self::with(text, Tone::Warning, false)
    }
    fn error(text: &'static str) -> Self {
        Self::with(text, Tone::Error, false)
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

fn muted_glyph(icon: Icon) -> Element<'static, Message> {
    widget::icon(icon.handle())
        .size(14)
        .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(|theme| {
            cosmic::iced::widget::svg::Style {
                color: Some(design::muted_icon_color(theme)),
            }
        })))
        .into()
}

/// How a row's status reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Plain,
    /// It went, but not all the way.
    Warning,
    /// It did not go.
    Error,
}

/// A tracked task's state is independent of the tool that launched it.
#[derive(Clone, Copy)]
enum BackgroundState {
    Working,
    Waiting,
    /// Alive, with its result so far delivered: nothing is asked of anyone.
    OnBackgroundWork,
    Failed,
    Stopped,
    Finished,
}

impl BackgroundState {
    fn caption(self) -> &'static str {
        match self {
            Self::Working => "Working",
            Self::Waiting => "Waiting for you",
            Self::OnBackgroundWork => "Waiting on background work",
            Self::Failed => "Failed",
            Self::Stopped => "Stopped",
            Self::Finished => "Finished",
        }
    }

    /// The row's status for this state: a spinner while the agent works,
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

    /// The name an agent's tab shows, where this conversation lists it.
    pub(super) fn listed_agent_name(&self, address: &str) -> Option<String> {
        self.subagent(address)
            .map(|agent| self.child_label(agent, 80).into_owned())
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
    /// shows the tool call, which `raw` then draws beneath it.
    pub(super) fn coordination_card<'a>(
        &self,
        item: &chat::Item,
        coordinated: &Coordinated,
        raw: Option<Element<'a, Message>>,
    ) -> Element<'a, Message> {
        let reported = item.content.status().unwrap_or(ToolStatus::Succeeded);
        let status = coordinated.reading.status(reported);
        let output = match &item.content {
            ItemContent::Command { output, .. } | ItemContent::Tool { output, .. } => {
                output.as_str()
            }
            _ => "",
        };
        let row = match &coordinated.reading.kind {
            Kind::Message(message) => self.message_row(coordinated, message, status, output),
            Kind::Spawn(spawn) => self.spawn_row(coordinated, spawn, status, output),
            Kind::Act(_) => return widget::Space::new().into(),
        };
        let row = self.row_view(item, row);
        match raw {
            Some(raw) => widget::column::with_children(vec![
                row,
                widget::container(raw)
                    .padding(iced::Padding::from([0.0, step::GAP_S]).bottom(step::GAP_S))
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(step::GAP_XS)
            .width(Length::Fill)
            .into(),
            None => row,
        }
    }

    fn message_row(
        &self,
        coordinated: &Coordinated,
        message: &Outbound,
        status: ToolStatus,
        output: &str,
    ) -> Row {
        let reading = &coordinated.reading;
        let recipient = recipient(message, reading, &|id| self.listed_agent_name(id));
        let listed: Vec<&chat::Subagent> = self.listed_subagents(coordinated).collect();
        let open = match (listed.as_slice(), &recipient.session) {
            ([agent], _) => Some(Action::SelectSubagent(self.target, Some(agent.id.clone()))),
            (_, Some(session)) if recipient.live => Some(Action::OpenSession(session.clone())),
            _ => None,
        };
        let (preview, full) = match &message.body {
            Text::Inline(body) => (one_line_of(body), body.trim().to_owned()),
            Text::File(path) => {
                let text = format!("The contents of {path}");
                (text.clone(), text)
            }
            _ => {
                let text = "Read from standard input".to_owned();
                (text.clone(), text)
            }
        };
        let receipt = reading.outcome.receipt.as_ref();
        let answered = !reading.outcome.messages.is_empty();
        let state = match (status, receipt) {
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
            (_, Some(receipt)) => RowStatus {
                detail: Some(arrival(receipt.arrival).to_owned()),
                ..RowStatus::plain(match receipt.arrival {
                    Arrival::WhenIdle | Arrival::NextSession => "Queued",
                    Arrival::Inbox | Arrival::User => "Delivered",
                })
            },
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
        };
        Row {
            glyph: Icon::Send,
            verb: message_verb(message.kind),
            label: recipient.plain(),
            preview,
            full,
            state,
            open,
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
        let label = listed
            .map(|agent| self.child_label(agent, 80).into_owned())
            .unwrap_or_else(|| started(spawn));
        let preview = match spawn.agents.as_slice() {
            [agent] if agent.kind.trim() != label => agent.kind.trim().to_owned(),
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
        };
        let observed_model = listed
            .filter(|_| !reading.cli)
            .and_then(|agent| agent.model.as_deref());
        let mut full: Vec<String> = spawn
            .agents
            .iter()
            .filter_map(|agent| agent.prompt.inline())
            .map(|prompt| prompt.trim().to_owned())
            .filter(|prompt| !prompt.is_empty())
            .collect();
        let details = spawn_details(reading, spawn, observed_model);
        if !details.is_empty() {
            full.push(details);
        }
        let background = self.background_state(coordinated);
        let run = reading.outcome.run.as_ref();
        let mut glyph = Icon::Agent;
        // Use the observed task state while a native launch tool is still
        // settling, but keep an explicit launch failure visible.
        let state = match background
            .filter(|_| matches!(status, ToolStatus::Running | ToolStatus::Succeeded))
        {
            Some(state) => {
                glyph = match state {
                    BackgroundState::Waiting => Icon::AgentWaiting,
                    BackgroundState::OnBackgroundWork => Icon::AgentBackground,
                    _ => Icon::Agent,
                };
                state.status()
            }
            None => match status {
                ToolStatus::Running if spawn.waits => RowStatus::live("Working"),
                ToolStatus::Running => RowStatus::live("Starting"),
                ToolStatus::Stopped => RowStatus::plain("Stopped"),
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
        Row {
            glyph,
            verb: "Started",
            label,
            preview,
            full: full.join("\n\n"),
            state,
            open: listed.map(|agent| Action::SelectSubagent(self.target, Some(agent.id.clone()))),
        }
    }

    /// One coordination row, on the step list's grid.
    fn row_view<'a>(&self, item: &chat::Item, row: Row) -> Element<'a, Message> {
        let failed = row.state.tone == Tone::Error;
        let lead: Element<'static, Message> = if failed {
            widget::icon(Icon::Error.handle())
                .size(14)
                .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(|theme| {
                    cosmic::iced::widget::svg::Style {
                        color: Some(design::destructive_color(theme)),
                    }
                })))
                .into()
        } else {
            muted_glyph(row.glyph)
        };
        // A long name is cut so the start of the message keeps its place.
        let label = Self::short_description(&row.label, LABEL_CHARS);
        let mut line: Vec<Element<'static, Message>> = vec![
            lead,
            design::body(row.verb)
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
            chat_title(label)
                .size(design::TEXT_BODY)
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
            step::one_line(
                design::body(row.preview.clone())
                    .class(cosmic::theme::Text::Custom(design::muted_text)),
            ),
        ];
        if row.state.live {
            line.push(crate::ui_controls::activity_spinner(
                14.0,
                design::accent_color,
            ));
        }
        let status = design::body(row.state.text).wrapping(iced::widget::text::Wrapping::None);
        line.push(
            match row.state.tone {
                Tone::Plain => status.class(cosmic::theme::Text::Custom(design::muted_text)),
                Tone::Warning => status.class(cosmic::theme::Text::Custom(design::warning_text)),
                Tone::Error => status.class(cosmic::theme::Text::Custom(design::destructive_text)),
            }
            .into(),
        );
        // What the tooltip and a screen reader give whole: the outcome's
        // reason first, then the message or prompt.
        let mut tip = Vec::new();
        if let Some(detail) = &row.state.detail {
            tip.push(match row.state.tone {
                Tone::Error => format!("{}: {detail}", row.state.text),
                _ => detail.clone(),
            });
        }
        if !row.full.is_empty() {
            tip.push(bounded(&row.full));
        }
        let tip = tip.join("\n\n");
        let name = [
            format!("{} {}", row.verb, row.label),
            row.state.text.to_owned(),
            row.preview,
        ]
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        let content = widget::row::with_children(line)
            .spacing(step::GAP_S)
            .align_y(Alignment::Center)
            .width(Length::Fill);
        let open_tool = self.item_expanded(&item.id);
        let source = match &item.content {
            ItemContent::Command { .. } if open_tool => "Hide command",
            ItemContent::Command { .. } => "Show command",
            _ if open_tool => "Hide tool call",
            _ => "Show tool call",
        };
        let (target, id) = (self.target, item.id.clone());
        let menu = move || {
            let command = |copy| RowCommand {
                target,
                id: &id,
                copy,
            };
            widget::menu::items(
                &HashMap::new(),
                vec![
                    widget::menu::Item::Button("Copy message", None, command(true)),
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
        let row: Element<'static, Message> = match row.open {
            Some(open) => {
                let button = widget::button::custom(content)
                    .name(name)
                    .class(crate::ui_controls::control_style(false, None))
                    .padding(padding)
                    .width(Length::Fill)
                    .on_press(action(open));
                // The way in, drawn inside the row's highlight where the
                // pointer is. It only marks the row as one that opens: the row
                // is the control, so a click on the arrow is a click on the
                // row, and keyboard traversal and screen readers meet it once.
                let arrow = widget::container(muted_glyph(Icon::Jump))
                    .padding(iced::Padding {
                        right: step::GAP_S,
                        ..iced::Padding::ZERO
                    })
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(iced::alignment::Horizontal::Right)
                    .align_y(iced::alignment::Vertical::Center);
                hover::over(tipped(button.into()), arrow)
            }
            // An agent this conversation cannot open is only described.
            None => tipped(
                widget::container(content)
                    .padding(padding)
                    .width(Length::Fill)
                    .into(),
            ),
        };
        crate::interaction::context_menu_lazy(row, menu)
    }

    /// How the agents a step left running in the background are doing now,
    /// for as long as this conversation tracks them.
    fn background_state(&self, coordinated: &Coordinated) -> Option<BackgroundState> {
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
            Some(BackgroundState::OnBackgroundWork)
        } else if any(ToolStatus::Failed) {
            Some(BackgroundState::Failed)
        } else if any(ToolStatus::Stopped) || any(ToolStatus::Denied) {
            Some(BackgroundState::Stopped)
        } else {
            Some(BackgroundState::Finished)
        }
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
    ) -> Element<'static, Message> {
        let key = format!("{}#{part}", item.id);
        let open = self.part_unfolded(&key);
        let toggle =
            widget::button::custom(design::caption(if open { "Show less" } else { unfold }))
                .class(crate::ui_controls::control_style(false, None))
                .on_press(action(Action::Unfold(self.target, key)));
        fold::fold(
            prose::content_view(cached, content, settings, self.target, self.find.as_deref()),
            toggle,
            !open,
            view::DELIVERY_FOLDED_HEIGHT,
            step::GAP_S,
            design::raised,
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
                    muted_glyph(Icon::Chat),
                    design::caption(caption).into(),
                ])
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center)
                .into(),
                self.folded_markdown(item, cached, part, content, "Show more", settings),
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
}
