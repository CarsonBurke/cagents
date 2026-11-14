//! Drawing what an agent did to work with other agents.
//!
//! A message one agent sends another is the mirror of the delivery the other
//! receives, and is drawn as one: a card that names its recipient and shows
//! the message as prose, instead of a `Ran` row whose message is a quoted
//! shell argument. Starting an agent is drawn the same way, its prompt being
//! the part worth reading. Every other coordinating step stays one line of
//! the step strip, but says what it did - "Waited for reviewer" - rather than
//! reprinting the command that did it.
//!
//! The raw command and its output stay one disclosure away on every card.

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

    pub(super) fn is_spawn_step(&self) -> bool {
        self.reading.cli && matches!(self.reading.kind, Kind::Spawn(_))
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

/// Whom started agents are, as one phrase.
fn started(spawn: &Spawn) -> String {
    match spawn.agents.as_slice() {
        [only] => [&only.name, &only.kind]
            .into_iter()
            .find(|name| !name.is_empty())
            .map_or_else(|| "an agent".to_owned(), |name| name.clone()),
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

/// How a run stands, and which agent runs it, as one status line.
fn run_line(run: &Run) -> Option<(String, Tone)> {
    let agent = (!run.agent.is_empty()).then(|| format!("agent {}", run.agent));
    let tone = match run.state {
        Some(State::Failed | State::Interrupted) => Tone::Error,
        Some(State::Cancelled) => Tone::Warning,
        _ => Tone::Plain,
    };
    let state = run.state.map(|state| {
        let mut state = state_phrase(state).to_owned();
        if run.timed_out {
            state.push_str(", wait timed out");
        }
        state
    });
    match (agent, state) {
        (Some(agent), Some(state)) => Some((format!("{state} as {agent}"), tone)),
        (Some(agent), None) => Some((format!("Launch accepted as {agent}"), tone)),
        (None, Some(state)) => Some((state, tone)),
        (None, None) => None,
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

/// Describe only metadata reported for this launch; native children do not
/// inherit the parent model merely because they share a provider.
fn spawn_identity(reading: &Coordination, spawn: &Spawn, observed_model: Option<&str>) -> String {
    let mut parts = vec![
        if reading.cli {
            "cagents agent"
        } else {
            "Native subagent"
        }
        .to_owned(),
    ];
    if !spawn.provider.is_empty() {
        parts.push(spawn.provider.clone());
    }
    parts.push(
        observed_model
            .filter(|model| !model.trim().is_empty())
            .unwrap_or(if spawn.model.is_empty() {
                "Model not reported"
            } else {
                &spawn.model
            })
            .to_owned(),
    );
    if !spawn.effort.is_empty() {
        parts.push(spawn.effort.clone());
    }
    // A channel the agent went to other than this conversation's is named
    // on the card's first line, by its title; an ID would say nothing here.
    if reading.cli
        && let Some(crate::agent_execution::Placement::Worktree { path, .. }) = &spawn.placement
    {
        parts.push(format!("Worktree view {}", path.display()));
    }
    parts.join(" · ")
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

/// How a card's status line reads.
#[derive(Clone, Copy)]
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
    Ended,
    Finished,
}

impl BackgroundState {
    fn caption(self) -> &'static str {
        match self {
            Self::Working => "Working",
            Self::Waiting => "Waiting",
            Self::OnBackgroundWork => "Waiting on background work",
            Self::Failed => "Failed",
            Self::Stopped => "Stopped",
            Self::Ended => "Ended",
            Self::Finished => "Finished",
        }
    }

    fn tool_status(self) -> ToolStatus {
        match self {
            Self::Working | Self::Waiting => ToolStatus::Running,
            Self::OnBackgroundWork => ToolStatus::Waiting,
            Self::Failed => ToolStatus::Failed,
            Self::Stopped => ToolStatus::Stopped,
            Self::Ended => ToolStatus::Ended,
            Self::Finished => ToolStatus::Succeeded,
        }
    }
}

fn status_caption(text: String, tone: Tone) -> Element<'static, Message> {
    let caption = design::caption(text);
    match tone {
        Tone::Plain => caption.into(),
        Tone::Warning => caption
            .class(cosmic::theme::Text::Custom(design::warning_text))
            .into(),
        Tone::Error => caption
            .class(cosmic::theme::Text::Custom(design::destructive_text))
            .into(),
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

    /// A message or a started agent, drawn as a card that shows its text.
    ///
    /// `raw` is the command and its output, drawn under the card while the
    /// reader has it open.
    pub(super) fn coordination_card<'a>(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        coordinated: &Coordinated,
        settings: markdown::Settings,
        raw: Option<Element<'a, Message>>,
    ) -> Element<'a, Message> {
        let reported = item.content.status().unwrap_or(ToolStatus::Succeeded);
        let status = coordinated.reading.status(reported);
        if coordinated.reading.cli
            && let Kind::Spawn(spawn) = &coordinated.reading.kind
        {
            return self.spawn_step(item, cached, coordinated, spawn, status, settings, raw);
        }
        let output = match &item.content {
            ItemContent::Command { output, .. } | ItemContent::Tool { output, .. } => {
                output.as_str()
            }
            _ => "",
        };
        let mut rows: Vec<Element<'a, Message>> = Vec::new();
        match &coordinated.reading.kind {
            Kind::Message(message) => self.message_rows(
                &mut rows,
                item,
                cached,
                coordinated,
                message,
                status,
                output,
                settings,
            ),
            Kind::Spawn(spawn) => self.spawn_rows(
                &mut rows,
                item,
                cached,
                coordinated,
                spawn,
                status,
                output,
                settings,
            ),
            Kind::Act(_) => {}
        }
        rows.push(self.card_footer(item, cached, coordinated, status != reported));
        rows.extend(raw);
        let card = widget::container(
            widget::column::with_children(rows)
                .spacing(step::GAP_S)
                .width(Length::Fill),
        )
        // Raised where a delivery is sunken: what this conversation sent
        // reads as the other side of what it received.
        .class(design::surface(
            Some(design::raised),
            [design::RADIUS_M; 4],
            true,
        ))
        .padding([step::GAP_S as u16, step::GAP_M as u16])
        .width(Length::Fill);
        widget::container(view::message_actions(
            card.into(),
            self.message_copy(item),
            Alignment::Start,
        ))
        .max_width(design::READABLE_MEASURE)
        .width(Length::Fill)
        .into()
    }

    /// A separate agent is one coordination step in its parent's conversation.
    /// Keep the launch details available without repeating them in the transcript.
    #[allow(clippy::too_many_arguments)]
    fn spawn_step<'a>(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        coordinated: &Coordinated,
        spawn: &Spawn,
        status: ToolStatus,
        settings: markdown::Settings,
        raw: Option<Element<'a, Message>>,
    ) -> Element<'a, Message> {
        let expanded = self.item_expanded(&item.id);
        let name = started(spawn);
        let task = spawn.agents.first().and_then(|agent| {
            agent
                .prompt
                .inline()
                .filter(|text| !text.trim().is_empty())
                .or_else(|| (!agent.description.is_empty()).then_some(agent.description.as_str()))
        });
        let mut title = Self::short_description(&name, 60);
        if let Some(task) = task {
            title.push_str(" · ");
            let task = Self::short_description(task, 160);
            title.push_str(task.lines().next().unwrap_or(&task));
        }
        let summary = widget::container(
            design::caption(Self::short_description(&title, 200))
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::None)
                .ellipsize(iced::advanced::text::Ellipsize::End(
                    iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                )),
        )
        .width(Length::Fill)
        .clip(true);
        let disclosure = widget::button::custom(
            widget::row::with_children(vec![
                self.card_indicator(status, Icon::Agent),
                summary.into(),
            ])
            .spacing(step::GAP_S)
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .class(crate::ui_controls::control_style(expanded, None))
        .padding([step::GAP_XS as u16, step::GAP_S as u16])
        .name(if expanded {
            "Hide spawn details"
        } else {
            "Show spawn details"
        })
        .on_press(action(Action::Toggle(self.target, item.id.clone())));
        let mut heading = vec![crate::interaction::tooltip(disclosure, title).into()];
        if let Some(placed) = coordinated
            .reading
            .outcome
            .run
            .as_ref()
            .and_then(|run| run.placed_in.as_ref())
        {
            let destination = placed.title.as_ref().map_or_else(
                || "in another channel".to_owned(),
                |title| format!("in channel \u{201c}{title}\u{201d}"),
            );
            // Reserve the destination independently of a long task, as the
            // separate channel is what the person needs to locate here.
            heading.push(
                crate::interaction::tooltip(
                    widget::container(
                        design::caption(Self::short_description(&destination, 80))
                            .width(Length::Fill)
                            .wrapping(iced::widget::text::Wrapping::None)
                            .ellipsize(iced::advanced::text::Ellipsize::End(
                                iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                            )),
                    )
                    .width(Length::Fixed(180.0))
                    .clip(true),
                    destination,
                )
                .into(),
            );
        }
        if let Some(run) = &coordinated.reading.outcome.run
            && !run.agent.is_empty()
        {
            heading.push(
                widget::button::custom(design::caption("Open"))
                    .class(crate::ui_controls::control_style(false, None))
                    .padding([step::GAP_XS as u16, step::GAP_S as u16])
                    .name(format!("Open agent {}", run.agent))
                    .on_press(action(Action::OpenAgent(run.agent.clone())))
                    .into(),
            );
        }
        let mut rows = vec![
            widget::row::with_children(heading)
                .align_y(Alignment::Center)
                .into(),
        ];
        if expanded {
            let output = match &item.content {
                ItemContent::Command { output, .. } | ItemContent::Tool { output, .. } => {
                    output.as_str()
                }
                _ => "",
            };
            self.spawn_rows(
                &mut rows,
                item,
                cached,
                coordinated,
                spawn,
                status,
                output,
                settings,
            );
            rows.extend(raw);
        }
        widget::container(widget::column::with_children(rows).spacing(step::GAP_S))
            .class(design::surface(
                Some(design::raised),
                [design::RADIUS_M; 4],
                false,
            ))
            .width(Length::Fill)
            .into()
    }

    /// The glyph or spinner a card's heading opens with.
    fn card_indicator(&self, status: ToolStatus, icon: Icon) -> Element<'static, Message> {
        match status {
            ToolStatus::Running => crate::ui_controls::activity_spinner(14.0, design::accent_color),
            ToolStatus::Failed => widget::icon(Icon::Error.handle())
                .size(14)
                .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(|theme| {
                    cosmic::iced::widget::svg::Style {
                        color: Some(design::destructive_color(theme)),
                    }
                })))
                .into(),
            _ => muted_glyph(icon),
        }
    }

    /// A recipient's name and session, the session a link to its pane
    /// while it runs.
    fn session_link(&self, recipient: &Recipient) -> Vec<Element<'static, Message>> {
        let mut parts: Vec<Element<'static, Message>> = Vec::new();
        if !recipient.name.is_empty() {
            parts.push(chat_title(recipient.name.clone()).into());
        }
        let Some(session) = &recipient.session else {
            return parts;
        };
        let shown = short_id(session);
        let label: Element<'static, Message> = if recipient.name.is_empty() {
            chat_title(format!("session {shown}")).into()
        } else {
            design::caption(shown.to_owned())
                .font(iced::Font::MONOSPACE)
                .into()
        };
        parts.push(if recipient.live {
            widget::button::custom(label)
                .class(crate::ui_controls::control_style(false, None))
                .padding([0, step::GAP_XS as u16])
                .name(format!("Go to session {shown}"))
                .on_press(action(Action::OpenSession(session.clone())))
                .into()
        } else {
            label
        });
        parts
    }

    #[allow(clippy::too_many_arguments)]
    fn message_rows(
        &self,
        rows: &mut Vec<Element<'_, Message>>,
        item: &chat::Item,
        cached: &CachedItem,
        coordinated: &Coordinated,
        message: &Outbound,
        status: ToolStatus,
        output: &str,
        settings: markdown::Settings,
    ) {
        let reading = &coordinated.reading;
        let recipient = recipient(message, reading, &|id| self.listed_agent_name(id));
        let mut heading = vec![
            self.card_indicator(status, Icon::Send),
            design::caption(message_verb(message.kind)).into(),
        ];
        heading.extend(self.session_link(&recipient));
        rows.push(
            widget::row::with_children(heading)
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center)
                .wrap()
                .into(),
        );
        // An agent's tab names it by its type, which several agents share;
        // the task it was given tells this one apart. It sits under the
        // heading so a long task never crowds the recipient out of it.
        // Where no task does, the sender's own summary of the body says
        // what the message is about.
        let task = match self
            .listed_subagents(coordinated)
            .collect::<Vec<_>>()
            .as_slice()
        {
            [agent] => Some(agent.description.trim())
                .filter(|task| !task.is_empty() && *task != recipient.name),
            _ => None,
        };
        let caption = match task {
            Some(task) => Self::short_description(task, 80),
            None => message.summary.trim().to_owned(),
        };
        if !caption.is_empty() {
            rows.push(design::caption(caption).into());
        }
        rows.extend(self.text_block(
            item,
            cached,
            "body",
            &message.body,
            coordinated.prompts.first().and_then(Option::as_ref),
            "Show full message",
            settings,
        ));
        // How it went: where it is headed, or what stopped it.
        let receipt = reading.outcome.receipt.as_ref();
        let replies = &reading.outcome.messages;
        let line = match (status, receipt) {
            (ToolStatus::Running, _) if message.waits => {
                Some(("Waiting for an answer…".to_owned(), Tone::Plain))
            }
            (ToolStatus::Running, _) => Some(("Sending…".to_owned(), Tone::Plain)),
            // An ask that timed out was still sent; only the answer is late.
            (ToolStatus::Failed, Some(receipt)) if message.waits => Some((
                format!(
                    "{} · No answer yet; it will arrive as a message",
                    arrival(receipt.arrival)
                ),
                Tone::Warning,
            )),
            // An ask prints its receipt only once its wait ends, so one
            // stopped while waiting was sent without saying so.
            (ToolStatus::Stopped, _) if message.waits => {
                Some(("Stopped waiting for an answer".to_owned(), Tone::Warning))
            }
            (ToolStatus::Stopped, None) => Some((
                "Stopped before it confirmed sending".to_owned(),
                Tone::Warning,
            )),
            (ToolStatus::Failed | ToolStatus::Denied | ToolStatus::Stopped, _) => {
                let reason = failure(reading, output);
                Some((
                    if reason.is_empty() {
                        "Not sent".to_owned()
                    } else {
                        format!("Not sent: {reason}")
                    },
                    Tone::Error,
                ))
            }
            (_, Some(receipt)) if replies.is_empty() => {
                Some((arrival(receipt.arrival).to_owned(), Tone::Plain))
            }
            // A prompt to an agent starts a run of it.
            _ => reading
                .outcome
                .delivery
                .map(|delivery| (delivered(delivery).to_owned(), Tone::Plain))
                .or_else(|| reading.outcome.run.as_ref().and_then(run_line)),
        };
        if let Some((text, tone)) = line {
            rows.push(status_caption(text, tone));
        }
        // What an ask or a waiting reply collected answers it; what a
        // prompt's wait collected merely arrived while it waited.
        let heading = match message.kind {
            OutboundKind::Ask | OutboundKind::Reply => "Answer from",
            _ => "From",
        };
        rows.extend(self.arrived(item, cached, coordinated, heading, settings));
        rows.extend(self.result(item, cached, coordinated, settings));
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_rows(
        &self,
        rows: &mut Vec<Element<'_, Message>>,
        item: &chat::Item,
        cached: &CachedItem,
        coordinated: &Coordinated,
        spawn: &Spawn,
        status: ToolStatus,
        output: &str,
        settings: markdown::Settings,
    ) {
        let reading = &coordinated.reading;
        let background = self.background_state(coordinated);
        // Use the observed task state while a native launch tool is still
        // settling, but keep an explicit launch failure visible.
        let indicator = match background
            .filter(|_| matches!(status, ToolStatus::Running | ToolStatus::Succeeded))
        {
            Some(BackgroundState::Waiting) => muted_glyph(Icon::AgentWaiting),
            Some(BackgroundState::OnBackgroundWork) => muted_glyph(Icon::AgentBackground),
            Some(state) => self.card_indicator(state.tool_status(), Icon::Agent),
            None => self.card_indicator(status, Icon::Agent),
        };
        // The agent is named as its tab names it. Its task describes it in
        // the tooltip but never stands in for an absent name; agent origin
        // and launch options belong on their own secondary line.
        let primary = match reading.subagents.as_slice() {
            [id] => self.listed_agent_name(id),
            _ => None,
        }
        .unwrap_or_else(|| started(spawn));
        let task = match spawn.agents.as_slice() {
            [agent] if !agent.description.trim().is_empty() => agent.description.clone(),
            _ => primary.clone(),
        };
        let mut heading = vec![
            indicator,
            crate::interaction::tooltip(
                chat_title(Self::short_description(
                    primary
                        .lines()
                        .map(str::trim)
                        .find(|line| !line.is_empty())
                        .unwrap_or(&primary),
                    80,
                )),
                task,
            )
            .into(),
        ];
        let run = reading.outcome.run.as_ref();
        if let Some(placed) = run.and_then(|run| run.placed_in.as_ref()) {
            heading.push(
                design::caption(match &placed.title {
                    Some(title) => format!("in channel \u{201c}{title}\u{201d}"),
                    None => "in another channel".to_owned(),
                })
                .into(),
            );
        }
        heading.push(widget::Space::new().width(Length::Fill).into());
        // A separate session has a tab of its own, which this opens.
        if reading.cli
            && let Some(agent) = run.map(|run| run.agent.as_str())
            && !agent.is_empty()
            && !matches!(status, ToolStatus::Failed | ToolStatus::Denied)
        {
            heading.push(
                widget::button::custom(
                    widget::row::with_children(vec![
                        design::caption("Open").into(),
                        muted_glyph(Icon::Jump),
                    ])
                    .spacing(step::GAP_XS)
                    .align_y(Alignment::Center),
                )
                .class(crate::ui_controls::control_style(false, None))
                .padding([step::GAP_XS as u16, step::GAP_S as u16])
                .name(format!("Open agent {agent}"))
                .on_press(action(Action::OpenAgent(agent.to_owned())))
                .into(),
            );
        }
        rows.push(
            widget::row::with_children(heading)
                .spacing(step::GAP_S)
                .align_y(Alignment::Center)
                .into(),
        );
        let observed_model = if !reading.cli {
            match reading.subagents.as_slice() {
                [id] => self.subagent(id).and_then(|agent| agent.model.as_deref()),
                _ => None,
            }
        } else {
            None
        };
        let kind = match spawn.agents.as_slice() {
            [agent] if !agent.kind.is_empty() && agent.kind != primary => {
                format!(" · {}", agent.kind)
            }
            _ => String::new(),
        };
        rows.push(
            design::caption(format!(
                "{}{}",
                spawn_identity(reading, spawn, observed_model),
                kind,
            ))
            .into(),
        );
        if !spawn.directory.is_empty() {
            rows.push(
                design::caption(format!("in {}", spawn.directory))
                    .font(iced::Font::MONOSPACE)
                    .into(),
            );
        }
        if let Some(brief) = &coordinated.brief {
            rows.push(self.folded_markdown(item, cached, "brief", brief, "Shared brief", settings));
        }
        let several = spawn.agents.len() > 1;
        for (index, (agent, prompt)) in spawn.agents.iter().zip(&coordinated.prompts).enumerate() {
            let mut title: Vec<String> = Vec::new();
            if several {
                title.extend(
                    [&agent.name, &agent.kind]
                        .into_iter()
                        .filter(|name| !name.is_empty())
                        .cloned(),
                );
            }
            if !agent.description.is_empty() && (several || agent.description != primary) {
                title.push(agent.description.clone());
            }
            if !title.is_empty() {
                rows.push(if several {
                    chat_title(title.join(" · ")).into()
                } else {
                    design::caption(title.join(" · ")).into()
                });
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
        // How the run went, as far as this step saw it.
        let line = match status {
            ToolStatus::Running if background.is_some() => None,
            ToolStatus::Running if spawn.waits => Some(("Working…".to_owned(), Tone::Plain)),
            ToolStatus::Running => Some(("Starting…".to_owned(), Tone::Plain)),
            ToolStatus::Failed | ToolStatus::Denied | ToolStatus::Stopped => {
                let reason = failure(reading, output);
                Some((
                    match status {
                        ToolStatus::Stopped => "Stopped".to_owned(),
                        _ if reason.is_empty() || run.is_some() => "Failed".to_owned(),
                        _ => format!("Failed: {reason}"),
                    },
                    Tone::Error,
                ))
            }
            _ => run.and_then(run_line),
        };
        if let Some((text, tone)) = line {
            rows.push(status_caption(text, tone));
        }
        rows.extend(self.arrived(item, cached, coordinated, "From", settings));
        rows.extend(self.result(item, cached, coordinated, settings));
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
        } else if any(ToolStatus::Ended) {
            Some(BackgroundState::Ended)
        } else {
            Some(BackgroundState::Finished)
        }
    }

    /// What a run the step waited on answered.
    fn result(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        coordinated: &Coordinated,
        settings: markdown::Settings,
    ) -> Option<Element<'static, Message>> {
        let run = coordinated.reading.outcome.run.as_ref()?;
        let result = coordinated
            .answers
            .last()
            .filter(|_| !run.result.is_empty())?;
        Some(self.answer_block(
            item,
            cached,
            "result",
            "Result".to_owned(),
            result,
            settings,
        ))
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
            (Text::Inline(_), Some(content)) => {
                self.folded_markdown(item, cached, part, content, unfold, settings)
            }
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

    /// The card's last line: the subagent's transcript, where this
    /// conversation has it, and the command behind the card.
    fn card_footer(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        coordinated: &Coordinated,
        overruled: bool,
    ) -> Element<'static, Message> {
        let mut controls: Vec<Element<'static, Message>> = Vec::new();
        let listed: Vec<&chat::Subagent> = self.listed_subagents(coordinated).collect();
        for agent in &listed {
            let label = if listed.len() == 1 {
                "Transcript".to_owned()
            } else {
                format!("Open {}", self.child_label(agent, 80))
            };
            controls.push(
                widget::button::custom(
                    widget::row::with_children(vec![
                        design::caption(label).into(),
                        muted_glyph(Icon::Jump),
                    ])
                    .spacing(step::GAP_XS)
                    .align_y(Alignment::Center),
                )
                .class(crate::ui_controls::control_style(false, None))
                .padding([step::GAP_XS as u16, step::GAP_S as u16])
                .on_press(action(Action::SelectSubagent(
                    self.target,
                    Some(agent.id.clone()),
                )))
                .into(),
            );
        }
        let open = self.item_expanded(&item.id);
        let source = match &item.content {
            ItemContent::Command { .. } => "command",
            _ => "tool call",
        };
        controls.push(
            widget::button::custom(design::caption(if open {
                format!("Hide {source}")
            } else {
                format!("Show {source}")
            }))
            .class(crate::ui_controls::control_style(open, None))
            .padding([step::GAP_XS as u16, step::GAP_S as u16])
            .on_press(action(Action::Toggle(self.target, item.id.clone())))
            .into(),
        );
        // A step the CLI said failed is not "Completed" for its exit status,
        // and an agent left running in the background is not done because
        // its launch is.
        let detail = if overruled {
            Some("Failed".to_owned())
        } else if let Kind::Spawn(spawn) = &coordinated.reading.kind
            && coordinated.reading.cli
            && !spawn.waits
            && item.content.status() == Some(ToolStatus::Succeeded)
        {
            // Completing the launch command does not complete the separate
            // session. Report its observed state, or only the accepted launch.
            Some(
                match coordinated.reading.outcome.run.as_ref() {
                    Some(run) => run.state.map_or("Launch accepted", state_phrase),
                    None => "Launch command finished",
                }
                .to_owned(),
            )
        } else {
            self.background_state(coordinated)
                .map(|state| state.caption().to_owned())
                .or_else(|| cached.detail.clone())
        };
        if let Some(detail) = detail {
            controls.push(widget::Space::new().width(Length::Fill).into());
            let tone = match detail.as_str() {
                "Failed" => cosmic::theme::Text::Custom(design::destructive_text),
                "Waiting for you" => cosmic::theme::Text::Custom(design::warning_text),
                _ => cosmic::theme::Text::default(),
            };
            controls.push(
                design::heading(detail)
                    .size(design::TEXT_CAPTION)
                    .class(tone)
                    .into(),
            );
        }
        widget::row::with_children(controls)
            .spacing(step::GAP_XS)
            .align_y(Alignment::Center)
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
    fn launches_distinguish_native_children_and_separate_sessions_without_guessing() {
        let mut spawn = Spawn::default();
        let mut reading = Coordination {
            kind: Kind::Spawn(spawn.clone()),
            outcome: Default::default(),
            subagents: Vec::new(),
            cli: false,
        };
        assert_eq!(
            spawn_identity(&reading, &spawn, None),
            "Native subagent · Model not reported"
        );
        assert_eq!(
            spawn_identity(&reading, &spawn, Some("gpt-child-reported")),
            "Native subagent · gpt-child-reported"
        );
        reading.cli = true;
        assert_eq!(
            spawn_identity(&reading, &spawn, None),
            "cagents agent · Model not reported"
        );
        spawn.provider = "codex".into();
        spawn.model = "gpt-reported".into();
        spawn.effort = "low".into();
        let channel = crate::model::ChannelId::new();
        spawn.channel = Some(channel);
        spawn.placement = Some(crate::agent_execution::Placement::Channel(channel));
        assert_eq!(
            spawn_identity(&reading, &spawn, None),
            "cagents agent · codex · gpt-reported · low",
            "a channel is named by title on the first line, never by its ID"
        );
        spawn.placement = Some(crate::agent_execution::Placement::Worktree {
            repository: "/repo".into(),
            path: "/repo/tree".into(),
        });
        assert_eq!(
            spawn_identity(&reading, &spawn, None),
            "cagents agent · codex · gpt-reported · low · Worktree view /repo/tree"
        );
    }
}
