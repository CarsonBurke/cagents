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
    Act, Arrival, Coordination, Kind, Outbound, OutboundKind, Run, Spawn, Text,
};

/// A coordinating step as read, with the Markdown its card draws, formed
/// once per change of its item rather than once per frame.
pub(super) struct Coordinated {
    pub(super) reading: Coordination,
    /// A message's body, or each started agent's prompt in order. `None`
    /// where the step does not show the text.
    prompts: Vec<Option<markdown::Content>>,
    /// The brief every started agent shares.
    brief: Option<markdown::Content>,
    /// Each message the step printed, or a started agent's result.
    answers: Vec<markdown::Content>,
}

impl Coordinated {
    pub(super) fn new(reading: Coordination) -> Self {
        let parse = |text: &Text| text.inline().map(markdown::Content::parse);
        let prompts = match &reading.kind {
            Kind::Message(message) => vec![parse(&message.body)],
            Kind::Spawn(spawn) => spawn
                .agents
                .iter()
                .map(|agent| parse(&agent.prompt))
                .collect(),
            Kind::Act(_) => Vec::new(),
        };
        let brief = match &reading.kind {
            Kind::Spawn(spawn) if !spawn.brief.trim().is_empty() => {
                Some(markdown::Content::parse(&spawn.brief))
            }
            _ => None,
        };
        let mut answers: Vec<markdown::Content> = reading
            .outcome
            .messages
            .iter()
            .map(|message| markdown::Content::parse(&message.body))
            .collect();
        if let Some(run) = &reading.outcome.run
            && !run.result.is_empty()
        {
            answers.push(markdown::Content::parse(&run.result));
        }
        Self {
            reading,
            prompts,
            brief,
            answers,
        }
    }

    /// Every piece of Markdown the card draws, for highlighting its fences.
    pub(super) fn contents(&self) -> impl Iterator<Item = &markdown::Content> {
        self.prompts
            .iter()
            .flatten()
            .chain(&self.brief)
            .chain(&self.answers)
    }

    pub(super) fn is_card(&self) -> bool {
        self.reading.is_card()
    }

    /// The text of each part the card folds, by the part's name.
    pub(super) fn parts(&self) -> Vec<(String, &str)> {
        let mut parts = Vec::new();
        match &self.reading.kind {
            Kind::Message(message) => parts.extend(message.body.inline().map(|body| ("body".to_owned(), body))),
            Kind::Spawn(spawn) => {
                parts.push(("brief".to_owned(), spawn.brief.as_str()));
                parts.extend(spawn.agents.iter().enumerate().filter_map(|(index, agent)| {
                    Some((format!("prompt-{index}"), agent.prompt.inline()?))
                }));
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
        parts.extend(outcome.run.as_ref().map(|run| ("result".to_owned(), run.result.as_str())));
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
                Act::Stop { task } => ("Stopped", format!("background task {task}")),
                Act::Output { task } => ("Read the output of", format!("task {task}")),
                Act::Linear { verb, subject } => (verb, subject.clone()),
                Act::Cli(path) => ("Ran", format!("cagents {path}").trim_end().to_owned()),
            },
        };
        (verb.to_owned(), label)
    }

    /// The step's line with agents named by their IDs, for a summary formed
    /// where the conversation's subagents are not to hand.
    pub(super) fn summary(&self) -> String {
        let (verb, label) = self.line(&|id| short_id(id).to_owned());
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
        OutboundKind::Reply => named(format!("message {}", short_id(&message.to))),
        OutboundKind::Prompt if !reading.subagents.is_empty() => named(
            reading
                .subagents
                .iter()
                .map(|agent| name(agent))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        OutboundKind::Prompt => named(format!("agent {}", message.to)),
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
    let agent = (!run.agent.is_empty()).then(|| format!("agent {}", run.agent));
    let tone = match run.state {
        Some(State::Failed | State::Interrupted) => Tone::Error,
        Some(State::Cancelled) => Tone::Warning,
        _ => Tone::Plain,
    };
    let state = run.state.map(|state| {
        let mut state = state_phrase(state).to_owned();
        if run.timed_out {
            state.push_str(", still running when the wait ended");
        }
        state
    });
    match (agent, state) {
        (Some(agent), Some(state)) => Some((format!("{state} as {agent}"), tone)),
        (Some(agent), None) => Some((format!("Running on its own as {agent}"), tone)),
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
    reading.outcome.error.as_deref().unwrap_or_else(|| last_line(output))
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
    /// What an agent of this conversation is called: its subagent's type or
    /// description where this conversation lists it, else its short ID.
    pub(super) fn agent_name(&self, id: &str) -> String {
        self.snapshot
            .subagents
            .iter()
            .find(|agent| agent.id == id)
            .map(|agent| {
                [&agent.kind, &agent.description]
                    .into_iter()
                    .find(|name| !name.is_empty())
                    .cloned()
                    .unwrap_or_else(|| short_id(id).to_owned())
            })
            .unwrap_or_else(|| short_id(id).to_owned())
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
            Kind::Message(message) => {
                self.message_rows(&mut rows, item, cached, coordinated, message, status, output, settings)
            }
            Kind::Spawn(spawn) => {
                self.spawn_rows(&mut rows, item, cached, coordinated, spawn, status, output, settings)
            }
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
            [
                design::RADIUS_L,
                design::RADIUS_L,
                design::RADIUS_L,
                design::RADIUS_XS,
            ],
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

    /// The glyph or spinner a card's heading opens with.
    fn card_indicator(&self, status: ToolStatus, icon: Icon) -> Element<'static, Message> {
        match status {
            ToolStatus::Running => crate::ui_controls::activity_spinner(14.0, design::warning_color),
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
            parts.push(design::heading(recipient.name.clone()).into());
        }
        let Some(session) = &recipient.session else {
            return parts;
        };
        let shown = short_id(session);
        let label: Element<'static, Message> = if recipient.name.is_empty() {
            design::heading(format!("session {shown}")).into()
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
        let name = |id: &str| self.agent_name(id);
        let recipient = recipient(message, reading, &name);
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
                format!("{} · No answer yet; it will arrive as a message", arrival(receipt.arrival)),
                Tone::Warning,
            )),
            // An ask prints its receipt only once its wait ends, so one
            // stopped while waiting was sent without saying so.
            (ToolStatus::Stopped, _) if message.waits => Some((
                "Stopped waiting for an answer".to_owned(),
                Tone::Warning,
            )),
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
        let mut heading = vec![
            self.card_indicator(status, Icon::Agent),
            design::caption("Started").into(),
            design::heading(started(spawn)).into(),
        ];
        // How it runs, as the agent asked for it.
        let traits: Vec<&str> = [
            spawn.provider.as_str(),
            spawn.model.as_str(),
            spawn.effort.as_str(),
        ]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect();
        if !traits.is_empty() {
            heading.push(design::caption(traits.join(" · ")).into());
        }
        rows.push(
            widget::row::with_children(heading)
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center)
                .wrap()
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
            rows.push(self.folded_markdown(item, cached, "brief", brief, "Show the shared brief", settings));
        }
        let several = spawn.agents.len() > 1;
        for (index, (agent, prompt)) in spawn.agents.iter().zip(&coordinated.prompts).enumerate() {
            let mut title: Vec<String> = Vec::new();
            if several {
                title.extend([&agent.name, &agent.kind].into_iter().filter(|name| !name.is_empty()).cloned());
            } else if !agent.name.is_empty() && !agent.kind.is_empty() {
                title.push(agent.kind.clone());
            }
            if !agent.description.is_empty() {
                title.push(agent.description.clone());
            }
            if !title.is_empty() {
                rows.push(design::body(title.join(" · ")).into());
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
        let run = reading.outcome.run.as_ref();
        let line = match status {
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
    fn background_state(&self, coordinated: &Coordinated) -> Option<&'static str> {
        let Kind::Spawn(spawn) = &coordinated.reading.kind else {
            return None;
        };
        if spawn.waits {
            return None;
        }
        let states: Vec<ToolStatus> = coordinated
            .reading
            .subagents
            .iter()
            .filter_map(|id| self.snapshot.subagents.iter().find(|agent| &agent.id == id))
            .map(|agent| agent.status)
            .collect();
        let any = |wanted: ToolStatus| states.contains(&wanted);
        if states.is_empty() {
            None
        } else if any(ToolStatus::Running) {
            Some("Working in the background")
        } else if any(ToolStatus::Failed) {
            Some("Failed")
        } else if any(ToolStatus::Stopped) || any(ToolStatus::Denied) {
            Some("Stopped")
        } else {
            Some("Finished")
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
        let result = coordinated.answers.last().filter(|_| !run.result.is_empty())?;
        Some(self.answer_block(item, cached, "result", "Result".to_owned(), result, settings))
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
            (Text::File(path), _) => design::caption(format!("The contents of {path}"))
                .into(),
            _ => design::caption("Read from standard input, which the transcript does not show").into(),
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
        let open = self.unfolded.contains(&key);
        let toggle = widget::button::custom(design::caption(if open { "Show less" } else { unfold }))
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
                widget::row::with_children(vec![muted_glyph(Icon::Chat), design::caption(caption).into()])
                    .spacing(step::GAP_XS)
                    .align_y(Alignment::Center)
                    .into(),
                self.folded_markdown(item, cached, part, content, "Show all of it", settings),
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
        for id in &coordinated.reading.subagents {
            let Some(agent) = self.snapshot.subagents.iter().find(|agent| &agent.id == id) else {
                continue;
            };
            let label = if coordinated.reading.subagents.len() == 1 {
                "Open transcript".to_owned()
            } else {
                format!("Open {}", self.agent_name(&agent.id))
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
                .on_press(action(Action::SelectSubagent(self.target, Some(agent.id.clone()))))
                .into(),
            );
        }
        let open = self.expanded.contains(&item.id);
        let source = match &item.content {
            ItemContent::Command { .. } => "command",
            _ => "tool call",
        };
        controls.push(
            widget::button::custom(design::caption(if open {
                format!("Hide the {source}")
            } else {
                format!("Show the {source}")
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
        } else {
            self.background_state(coordinated)
                .map(str::to_owned)
                .or_else(|| cached.detail.clone())
        };
        if let Some(detail) = detail {
            controls.push(widget::Space::new().width(Length::Fill).into());
            controls.push(design::caption(detail).into());
        }
        widget::row::with_children(controls)
            .spacing(step::GAP_XS)
            .align_y(Alignment::Center)
            .width(Length::Fill)
            .into()
    }
}
