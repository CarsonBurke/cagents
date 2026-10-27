//! Drawing what an agent did to work with other agents.
//!
//! A message one agent sends another is the mirror of the delivery the other
//! receives, and is drawn as one: a row that names its recipient, instead of
//! a `Ran` row whose message is a quoted shell argument. Prompts and tool
//! payloads open intentionally; starting an agent does not paste its internal
//! instructions into the conversation. Every other coordinating step stays one line of
//! the step strip, but says what it did - "Waited for reviewer" - rather than
//! reprinting the command that did it.
//!
//! The raw command and its output stay one disclosure away on every card.

use super::*;
use crate::agent_execution::State;
use crate::chat::ToolStatus;
use crate::chat::coordination::{
    Act, Arrival, Coordination, Kind, Outbound, OutboundKind, Run, Spawn, Text,
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
    answer_summaries: Vec<String>,
    documents: Vec<prose_document::Data>,
    #[cfg(test)]
    pub(super) created_on: std::thread::ThreadId,
    #[cfg(test)]
    disposed: Option<std::sync::mpsc::Sender<std::thread::ThreadId>>,
}
impl Prepared {
    pub(super) fn prepare(reading: Coordination, cancelled: &AtomicBool) -> Result<Self, String> {
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
            Ok(markdown::Content::parse(text))
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
        let answer_summaries = reading
            .outcome
            .messages
            .iter()
            .map(|message| message.body.as_str())
            .chain(
                reading
                    .outcome
                    .run
                    .as_ref()
                    .filter(|run| !run.result.is_empty())
                    .map(|run| run.result.as_str()),
            )
            .map(concise_result)
            .collect();
        let mut prepared = Self {
            reading,
            prompts,
            brief,
            answers,
            answer_summaries,
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
        Self::prepare(reading, &AtomicBool::new(false)).unwrap()
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

    /// A completed delegation copies its authentic result; otherwise copy
    /// the message or prompt, never the surrounding transport receipt.
    pub(super) fn copy_text(&self) -> Option<String> {
        match &self.reading.kind {
            Kind::Message(message) => message.body.inline().map(str::to_owned),
            Kind::Spawn(spawn) => {
                if let Some(run) = &self.reading.outcome.run
                    && !run.result.is_empty()
                {
                    return Some(run.result.clone());
                }
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
            Kind::Spawn(_) => return None,
            Kind::Act(Act::Inbox { .. }) => Icon::Chat,
            Kind::Act(Act::Linear { .. }) => Icon::Comment,
            Kind::Act(Act::Sessions | Act::Whoami | Act::Agents) => Icon::Users,
            Kind::Act(Act::Stop { .. } | Act::Output { .. } | Act::Cli(_)) => return None,
            Kind::Act(_) => Icon::Agent,
        })
    }

    /// The step's verb and what it acted on, `name` naming an agent of this
    /// conversation by its ID.
    pub(super) fn line(&self, name: &dyn Fn(&str) -> String) -> (String, String) {
        let names = |agents: &[String]| {
            agents
                .iter()
                .map(|agent| name(agent))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let (verb, label): (&str, String) = match &self.reading.kind {
            Kind::Message(message) => (
                message_verb(message.kind),
                recipient(message, &self.reading, name).plain(),
            ),
            Kind::Spawn(spawn) => ("Started", started(spawn)),
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
                Act::Answer { agent, action, .. } => (
                    "Answered",
                    format!("{}'s request with {action}", name(agent)),
                ),
                Act::Interrupt { agent } => ("Interrupted", name(agent)),
                Act::Close { agents } => ("Closed", names(agents)),
                Act::Resume { agents } => ("Resumed", names(agents)),
                Act::Stop { .. } => ("Stopped", "background task".into()),
                Act::Output { .. } => ("Read the output of", "background task".into()),
                Act::Linear { verb, subject } => (verb, subject.clone()),
                Act::Cli(path) => ("Ran", format!("cagents {path}").trim_end().to_owned()),
            },
        };
        (verb.to_owned(), label)
    }

    /// The step's line with agents named by their IDs, for a summary formed
    /// where the conversation's subagents are not to hand.
    pub(super) fn summary(&self) -> String {
        let (verb, label) = self.line(&|_| "agent".to_owned());
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

/// Prepared beside Markdown, never by laying out a clipped full report.
fn concise_result(text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut chars = first.chars();
    let mut summary: String = chars.by_ref().take(160).collect();
    if chars.next().is_some() {
        summary.push('…');
    }
    summary
}

/// Whom started agents are, as one phrase.
fn started(spawn: &Spawn) -> String {
    match spawn.agents.as_slice() {
        [only] if !only.name.trim().is_empty() => only.name.clone(),
        [_] => "an agent".to_owned(),
        agents => format!("{} agents", agents.len()),
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
            (Some(_), true) => "conversation".to_owned(),
            (Some(_), false) => self.name.clone(),
            (None, _) => self.name.clone(),
        }
    }
}

/// Whom `message` went to, by the CLI's own account where it printed one
/// and otherwise as the agent named them.
fn recipient(
    message: &Outbound,
    reading: &Coordination,
    name: &dyn Fn(&str) -> String,
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
    match message.kind {
        OutboundKind::Report => named("the conversation that started it".into()),
        OutboundKind::Reply => named("a message".to_owned()),
        OutboundKind::Prompt if !reading.subagents.is_empty() => named(
            reading
                .subagents
                .iter()
                .map(|agent| name(agent))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        OutboundKind::Prompt => named("an agent".to_owned()),
        _ if message.to == "parent" => named("the session that started it".into()),
        // A `cagents` recipient is a session ID, or a prefix of one.
        _ if reading.cli && is_identifier(&message.to) => Recipient {
            name: String::new(),
            session: Some(message.to.clone()),
            live: true,
        },
        _ => named(name(&message.to)),
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
    }
}

/// How a run stands, and which agent runs it, as one status line.
fn run_line(run: &Run) -> Option<(String, Tone)> {
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
    state.map(|state| (state, tone))
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
    Failed,
    Denied,
    Stopped,
    Finished,
}

impl BackgroundState {
    fn caption(self) -> &'static str {
        match self {
            Self::Working => "Working",
            Self::Waiting => "Waiting for you",
            Self::Failed => "Failed",
            Self::Denied => "Denied",
            Self::Stopped => "Stopped",
            Self::Finished => "Finished",
        }
    }

    fn tool_status(self) -> ToolStatus {
        match self {
            Self::Working | Self::Waiting => ToolStatus::Running,
            Self::Failed => ToolStatus::Failed,
            Self::Denied => ToolStatus::Denied,
            Self::Stopped => ToolStatus::Stopped,
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
    /// A reported name is identity; task prose and transport IDs are details.
    pub(super) fn agent_name(&self, id: &str) -> String {
        self.snapshot
            .subagents
            .iter()
            .find(|agent| agent.id == id)
            .map(|agent| agent.kind.trim())
            .filter(|name| !name.is_empty())
            .unwrap_or("Agent")
            .to_owned()
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
        let spawn = matches!(coordinated.reading.kind, Kind::Spawn(_));
        if !spawn {
            rows.push(self.card_footer(item, cached, coordinated, status != reported));
        }
        if let Some(raw) = raw {
            let key = format!("{}#tool-call", item.id);
            let open = self.part_unfolded(&key)
                || self.part_unfolded(&format!("{}#input", item.id))
                || self.part_unfolded(&format!("{}#output", item.id));
            rows.push(
                widget::button::custom(design::caption(if open {
                    "Hide tool call"
                } else {
                    "Tool call"
                }))
                .name(format!(
                    "{} original tool call",
                    if open { "Hide" } else { "Show" }
                ))
                .id(card_control_id(
                    self.target.card,
                    &format!(
                        "coordination-{}-{}-tool-call",
                        self.target.generation, item.id
                    ),
                ))
                .class(crate::ui_controls::control_style(open, None))
                .on_press(action(Action::Unfold(self.target, key)))
                .into(),
            );
            if open {
                rows.push(raw);
            }
        }
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
        .padding(if spawn {
            [step::GAP_XS as u16, step::GAP_S as u16]
        } else {
            [step::GAP_S as u16, step::GAP_M as u16]
        })
        .width(Length::Fill);
        let card: Element<'a, Message> = if spawn {
            card.into()
        } else {
            view::message_actions(card.into(), self.message_copy(item), Alignment::Start)
        };
        widget::container(card)
            .max_width(design::READABLE_MEASURE)
            .width(Length::Fill)
            .into()
    }

    /// The glyph or spinner a card's heading opens with.
    fn card_indicator(&self, status: ToolStatus) -> Element<'static, Message> {
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
            ToolStatus::Succeeded => muted_glyph(Icon::Check),
            ToolStatus::Denied => muted_glyph(Icon::Unavailable),
            ToolStatus::Stopped => muted_glyph(Icon::Cancel),
        }
    }

    fn coordination_name(&self, coordinated: &Coordinated, spawn: &Spawn) -> String {
        let observed: Vec<_> = coordinated
            .reading
            .subagents
            .iter()
            .filter_map(|id| {
                self.snapshot
                    .subagents
                    .iter()
                    .find(|agent| &agent.id == id)
                    .map(|agent| agent.kind.trim())
                    .filter(|name| !name.is_empty())
            })
            .collect();
        let names = if observed.is_empty() {
            spawn
                .agents
                .iter()
                .map(|agent| agent.name.trim())
                .filter(|name| !name.is_empty())
                .collect()
        } else {
            observed
        };
        if names.is_empty() {
            "Agent".to_owned()
        } else {
            names.join(", ")
        }
    }

    fn coordination_status(&self, coordinated: &Coordinated, reported: ToolStatus) -> &'static str {
        if let Some(state) = self.background_state(coordinated) {
            return state.caption();
        }
        if let Kind::Spawn(spawn) = &coordinated.reading.kind
            && coordinated.reading.cli
            && !spawn.waits
            && reported == ToolStatus::Succeeded
        {
            return coordinated
                .reading
                .outcome
                .run
                .as_ref()
                .and_then(|run| run.state)
                .map_or("Launch accepted", state_phrase);
        }
        match coordinated.reading.status(reported) {
            ToolStatus::Running => "Working",
            ToolStatus::Succeeded => "Finished",
            ToolStatus::Failed => "Failed",
            ToolStatus::Denied => "Denied",
            ToolStatus::Stopped => "Stopped",
        }
    }

    /// A recipient's name and session, the session a link to its pane
    /// while it runs.
    fn session_link(&self, recipient: &Recipient) -> Vec<Element<'static, Message>> {
        let label = if recipient.name.is_empty() {
            "Conversation".to_owned()
        } else {
            recipient.name.clone()
        };
        let title = chat_title(label.clone());
        vec![if let Some(session) = &recipient.session
            && recipient.live
        {
            widget::button::custom(title)
                .class(crate::ui_controls::control_style(false, None))
                .padding([0, step::GAP_XS as u16])
                .name(format!("Open conversation {label}"))
                .on_press(action(Action::OpenSession(session.clone())))
                .into()
        } else {
            title.into()
        }]
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
        let name = |id: &str| self.agent_name(id);
        let recipient = recipient(message, reading, &name);
        let mut heading = vec![
            self.card_indicator(status),
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
        if !message.summary.is_empty() {
            rows.push(design::caption(message.summary.clone()).into());
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
            _ => reading.outcome.run.as_ref().and_then(run_line),
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
        let reported_run = reading
            .outcome
            .run
            .as_ref()
            .and_then(|run| run.state)
            .map(|state| match state {
                State::Accepted | State::Running | State::Waiting => ToolStatus::Running,
                State::Succeeded => ToolStatus::Succeeded,
                State::Failed | State::Interrupted => ToolStatus::Failed,
                State::Cancelled => ToolStatus::Stopped,
            });
        let indicator = match background
            .filter(|_| matches!(status, ToolStatus::Running | ToolStatus::Succeeded))
        {
            Some(BackgroundState::Waiting) => muted_glyph(Icon::AgentWaiting),
            Some(state) => self.card_indicator(state.tool_status()),
            None => self.card_indicator(if status == ToolStatus::Succeeded {
                reported_run.unwrap_or(status)
            } else {
                status
            }),
        };
        let names = self.coordination_name(coordinated, spawn);
        rows.push(
            widget::flex_row(vec![
                widget::row::with_children(vec![indicator, chat_title(names).into()])
                    .spacing(step::GAP_S)
                    .align_y(Alignment::Center)
                    .into(),
                self.card_footer(
                    item,
                    cached,
                    coordinated,
                    status != item.content.status().unwrap_or(status),
                ),
            ])
            .spacing(step::GAP_S as u16)
            .align_items(Alignment::Center)
            .into(),
        );

        // Instructions are available in full only after an intentional
        // disclosure. Find can independently open the exact existing part.
        let details = self.item_expanded(&item.id);
        if let Some(brief) = &coordinated.brief
            && (details || self.part_unfolded(&format!("{}#brief", item.id)))
        {
            rows.push(self.folded_markdown(item, cached, "brief", brief, "Shared brief", settings));
        }
        for (index, (agent, prompt)) in spawn.agents.iter().zip(&coordinated.prompts).enumerate() {
            let part = format!("prompt-{index}");
            if !details && !self.part_unfolded(&format!("{}#{part}", item.id)) {
                continue;
            }
            rows.push(
                design::caption(if spawn.agents.len() == 1 {
                    "Prompt".to_owned()
                } else if agent.name.trim().is_empty() {
                    format!("Prompt {}", index + 1)
                } else {
                    format!("Prompt for {}", agent.name)
                })
                .into(),
            );
            rows.extend(self.text_block(
                item,
                cached,
                &part,
                &agent.prompt,
                prompt.as_ref(),
                "Show prompt",
                settings,
            ));
        }
        if matches!(
            status,
            ToolStatus::Failed | ToolStatus::Denied | ToolStatus::Stopped
        ) {
            let reason = failure(reading, output);
            if !reason.is_empty() {
                rows.push(status_caption(reason.to_owned(), Tone::Error));
            }
        }
        // A successful launch is not a successful child. Keep the child's
        // actionable failure visible, without expanding its entire transcript.
        for child in coordinated
            .reading
            .subagents
            .iter()
            .filter_map(|id| self.snapshot.subagents.iter().find(|child| &child.id == id))
            .filter(|child| {
                matches!(
                    child.status,
                    ToolStatus::Failed | ToolStatus::Denied | ToolStatus::Stopped
                ) && !child.detail.trim().is_empty()
            })
        {
            let detail = if details {
                child.detail.clone()
            } else {
                concise_result(&child.detail)
            };
            rows.push(status_caption(
                format!("{}: {detail}", self.agent_name(&child.id)),
                Tone::Error,
            ));
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
        let agents: Vec<&chat::Subagent> = coordinated
            .reading
            .subagents
            .iter()
            .filter_map(|id| self.snapshot.subagents.iter().find(|agent| &agent.id == id))
            .collect();
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
        } else if any(ToolStatus::Failed) {
            Some(BackgroundState::Failed)
        } else if any(ToolStatus::Denied) {
            Some(BackgroundState::Denied)
        } else if any(ToolStatus::Stopped) {
            Some(BackgroundState::Stopped)
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
        Some(
            self.answer_block(
                item,
                cached,
                "result",
                "Result".to_owned(),
                result,
                coordinated
                    .answer_summaries
                    .last()
                    .map(String::as_str)
                    .unwrap_or_default(),
                matches!(coordinated.reading.kind, Kind::Spawn(_))
                    .then(|| Action::Copy(self.target, item.id.clone(), CopyPart::Text)),
                settings,
            ),
        )
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
        if self.item_expanded(&item.id) {
            return prose::content_view(
                cached,
                content,
                settings,
                self.target,
                self.find.as_deref(),
            );
        }
        let open = self.part_unfolded(&key);
        let toggle =
            widget::button::custom(design::caption(if open { "Show less" } else { unfold }))
                .class(crate::ui_controls::control_style(false, None))
                .name(format!(
                    "{} {part} details",
                    if open { "Hide" } else { "Show" }
                ))
                .id(card_control_id(
                    self.target.card,
                    &format!(
                        "coordination-{}-{}-part-{part}",
                        self.target.generation, item.id
                    ),
                ))
                .on_press(action(Action::Unfold(self.target, key)));
        let mut rows: Vec<Element<'static, Message>> = vec![toggle.into()];
        if open {
            rows.push(prose::content_view(
                cached,
                content,
                settings,
                self.target,
                self.find.as_deref(),
            ));
        }
        widget::column::with_children(rows)
            .spacing(step::GAP_XS)
            .into()
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
                    format!("{heading} {}", self.agent_name(&message.from)),
                    content,
                    coordinated
                        .answer_summaries
                        .get(index)
                        .map(String::as_str)
                        .unwrap_or_default(),
                    None,
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
        summary: &str,
        copy: Option<Action>,
        settings: markdown::Settings,
    ) -> Element<'static, Message> {
        let key = format!("{}#{part}", item.id);
        let open = self.item_expanded(&item.id) || self.part_unfolded(&key);
        let mut controls: Vec<Element<'static, Message>> = vec![design::caption(caption).into()];
        if !self.item_expanded(&item.id) {
            controls.push(
                widget::button::custom(design::caption(if open {
                    "Hide result"
                } else {
                    "Read result"
                }))
                .id(card_control_id(
                    self.target.card,
                    &format!("coordination-{}-{}-{part}", self.target.generation, item.id),
                ))
                .class(crate::ui_controls::control_style(open, None))
                .on_press(action(Action::Unfold(self.target, key)))
                .into(),
            );
        }
        if let Some(copy) = copy {
            controls.push(
                widget::button::custom(design::caption("Copy result"))
                    .name("Copy original agent result")
                    .id(card_control_id(
                        self.target.card,
                        &format!(
                            "coordination-{}-{}-{part}-copy",
                            self.target.generation, item.id
                        ),
                    ))
                    .class(crate::ui_controls::control_style(false, None))
                    .on_press(action(copy))
                    .into(),
            );
        }
        let mut rows = vec![
            widget::flex_row(controls)
                .spacing(step::GAP_XS as u16)
                .align_items(Alignment::Center)
                .into(),
        ];
        if !open && !summary.is_empty() {
            rows.push(design::body(summary.to_owned()).into());
        }
        if open {
            rows.push(prose::content_view(
                cached,
                content,
                settings,
                self.target,
                self.find.as_deref(),
            ));
        }
        widget::container(widget::column::with_children(rows).spacing(step::GAP_XS))
            .padding([step::GAP_XS as u16, 0])
            .width(Length::Fill)
            .into()
    }

    /// Related actions stay together; source IDs stay in their callbacks.
    fn card_footer(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        coordinated: &Coordinated,
        overruled: bool,
    ) -> Element<'static, Message> {
        let mut controls: Vec<Element<'static, Message>> = Vec::new();
        for id in &coordinated.reading.subagents {
            let Some(agent) = self.snapshot.subagents.iter().find(|agent| &agent.id == id) else {
                continue;
            };
            let name = self.agent_name(&agent.id);
            let label = if coordinated.reading.subagents.len() == 1 {
                "Transcript".to_owned()
            } else {
                format!("Open {name}")
            };
            controls.push(
                widget::button::custom(design::caption(label))
                    .name(format!("Open {name} transcript"))
                    .id(card_control_id(
                        self.target.card,
                        &format!(
                            "coordination-{}-{}-transcript-{}",
                            self.target.generation, item.id, agent.id
                        ),
                    ))
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
        let name = match &coordinated.reading.kind {
            Kind::Spawn(spawn) => self.coordination_name(coordinated, spawn),
            _ => "message".into(),
        };
        let state = self.coordination_status(
            coordinated,
            item.content.status().unwrap_or(ToolStatus::Succeeded),
        );
        controls.push(
            widget::button::custom(design::caption(if open {
                "Hide details"
            } else {
                "Details"
            }))
            .name(format!(
                "{} details for {name} ({state})",
                if open { "Hide" } else { "Show" }
            ))
            .id(card_control_id(
                self.target.card,
                &format!(
                    "coordination-{}-{}-details",
                    self.target.generation, item.id
                ),
            ))
            .class(crate::ui_controls::control_style(open, None))
            .padding([step::GAP_XS as u16, step::GAP_S as u16])
            .on_press(action(Action::Toggle(self.target, item.id.clone())))
            .into(),
        );
        if !matches!(coordinated.reading.kind, Kind::Spawn(_)) {
            if overruled {
                controls.push(status_caption("Failed".into(), Tone::Error));
            } else if let Some(detail) = &cached.detail {
                controls.push(design::caption(detail.clone()).into());
            }
        }
        widget::flex_row(controls)
            .spacing(step::GAP_XS as u16)
            .align_items(Alignment::Center)
            .into()
    }
}

#[cfg(test)]
mod presentation_tests {
    use super::*;

    #[test]
    fn result_preview_is_bounded_while_each_authentic_result_remains_exact() {
        let result = format!(
            "Verified Unicode λ result {}\nSecond authentic paragraph.",
            "x".repeat(300)
        );
        let reading = Coordination {
            kind: Kind::Act(Act::Agents),
            outcome: crate::chat::coordination::Outcome {
                run: Some(Run {
                    result: result.clone(),
                    ..Run::default()
                }),
                ..Default::default()
            },
            subagents: Vec::new(),
            cli: false,
        };
        let prepared = Prepared::new(reading);
        assert_eq!(prepared.answer_summaries[0].chars().count(), 161);
        assert!(prepared.answer_summaries[0].starts_with("Verified Unicode λ result"));
        assert_eq!(
            prepared.reading.outcome.run.as_ref().unwrap().result,
            result
        );
        assert_eq!(prepared.answers.len(), 1);
    }

    #[test]
    fn completed_agent_copy_is_its_exact_result_instead_of_prompt_or_receipt() {
        let result = "Distinct authentic result λ.\nSecond paragraph.";
        let coordinated = Coordinated::new(Coordination {
            kind: Kind::Spawn(Spawn {
                agents: vec![crate::chat::coordination::Delegate {
                    name: "Scout".into(),
                    kind: "internal-type".into(),
                    description: "Task prose".into(),
                    prompt: Text::Inline("Private prompt instructions".into()),
                }],
                ..Spawn::default()
            }),
            outcome: crate::chat::coordination::Outcome {
                run: Some(Run {
                    agent: "transport-id".into(),
                    result: result.into(),
                    ..Run::default()
                }),
                ..Default::default()
            },
            subagents: Vec::new(),
            cli: false,
        });
        assert_eq!(coordinated.copy_text().as_deref(), Some(result));
        assert_eq!(
            coordinated
                .parts()
                .iter()
                .find(|(part, _)| part == "prompt-0")
                .unwrap()
                .1,
            "Private prompt instructions",
            "disclosure keeps exact source independently of copy semantics"
        );
    }
}
