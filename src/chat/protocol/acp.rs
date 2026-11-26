//! The Agent Client Protocol, version 1: one chat adapter for every agent
//! that serves it over stdio, configured per agent by a [`Profile`].
//!
//! Wire reference: <https://agentclientprotocol.com>, schema v1 at
//! agentclientprotocol/agent-client-protocol `schema/v1/schema.json`,
//! checked against the `initialize` and `session/new` replies of
//! Antigravity 1.5.12 and Muse 1.4.23.
//!
//! cagents connects as a client that offers neither files nor terminals, so
//! the agent reads, writes and runs commands with its own tools, under its
//! own permission mode, and asks cagents only for approval. A conversation
//! goes `initialize`, then `session/new` or, to resume, `session/load`, then
//! the launch's model, reasoning and mode through the session's config
//! options; only then does it take prompts. Each prompt is one
//! `session/prompt` request whose reply ends the turn.
//!
//! A message queued while a turn runs waits in this adapter, and the next
//! turn starts with everything queued as soon as the running one ends, in
//! the same step, so nothing else can start a turn in between. A turn the
//! reader stopped, or one the agent refused to work on at all, leaves the
//! queue for the reader to start: their next message goes after it. An agent
//! whose profile [steers](Profile::steers) also takes a message mid-turn:
//! Antigravity 1.5.17 answers the running `session/prompt` at once, as
//! `end_turn`, and carries on with its work, background commands included,
//! under the new one, whose reply then ends the turn.
use super::{Adapter, ProcessSpec, Step};
use crate::chat::{
    self, Capabilities, Choice, Command, Event, Item, ItemContent, Launch, PlanState, PlanStep,
    PromptMode, Request, RequestKind, Role, SkillReach, SlashCommand,
};
use crate::notifications::Kind;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};

pub mod antigravity;
mod config;
mod history;
pub mod muse;
mod prompt;
mod rpc;
#[cfg(test)]
mod tests;
mod tools;

/// What an agent calls a conversation it has not named yet, as Muse's own
/// session list does.
const PLACEHOLDER_TITLES: &[&str] = &["New session"];
const MAX_TITLE_BYTES: usize = 4096;
/// Allow a deeper lookup when loading a saved conversation that may be old.
const MAX_SAVED_TITLE_LIST_PAGES: u8 = 64;
/// Keep repeated lookups for a conversation that just handled a prompt cheap.
const MAX_TURN_TITLE_LIST_PAGES: u8 = 4;
/// The ACP version this client speaks.
const PROTOCOL_VERSION: u64 = 1;
const MAX_COMMANDS: usize = 256;
const MAX_PLAN_STEPS: usize = 256;
const MAX_SESSION_ID: usize = 256;
/// The message identifiers remembered for naming items, about a long
/// conversation's worth.
const MAX_SEGMENT_IDS: usize = 4096;
/// The approvals that may be open at once; an agent asking for more is
/// refused rather than piling them up in front of the reader.
const MAX_PERMISSIONS: usize = 64;
/// The messages that may wait for the next turn, and the bytes their
/// prompts may hold between them, inline images included.
const MAX_QUEUED: usize = 32;
const MAX_QUEUED_BYTES: usize = 64 * 1024 * 1024;

fn title_text(value: &Value) -> Option<&str> {
    let title = value.as_str()?;
    let title = title[..title.floor_char_boundary(title.len().min(MAX_TITLE_BYTES))].trim();
    (!title.is_empty() && !PLACEHOLDER_TITLES.contains(&title)).then_some(title)
}

/// What one agent needs beyond the protocol itself.
pub struct Profile {
    /// The agent's name in what cagents tells the reader.
    pub label: &'static str,
    /// Turns the conversation's launch, the terminal command it was started
    /// as or would resume with, into the command that serves ACP and the
    /// session settings that command line chose.
    pub launch: fn(&Launch) -> Result<Startup, String>,
    /// The largest image a prompt may carry inline.
    pub max_image_bytes: u64,
    /// Each of the agent's own permission modes beside the cagents mode that
    /// means the same, which the reader and the CLI choose it by. A mode not
    /// listed keeps its own identifier.
    pub modes: &'static [(&'static str, &'static str)],
    /// Whether a model the agent lists is one cagents offers the reader,
    /// such as one that does not share their work beyond the conversation.
    pub offers_model: fn(&str) -> bool,
    /// Whether an error reply means the user has to sign in again. ACP
    /// reserves `-32000` for this; [`auth_required`] checks only that.
    pub sign_in_required: fn(&Value) -> bool,
    /// Whether a `session/prompt` sent while a turn runs reaches that turn
    /// without stopping it. ACP v1 does not say what an agent does with one;
    /// Antigravity 1.5.17 answers the running prompt at once and goes on
    /// with the new one, its tools and background commands still running.
    pub steers: bool,
    /// Whether `session/cancel` also ends the commands the agent left
    /// running in the background, as Antigravity 1.5.17's does.
    pub cancel_stops_background: bool,
    /// Whether a command the agent stops waiting for goes on in the
    /// background, the turn open until it ends, as Antigravity 1.5.17's
    /// `run_command` does past its `WaitMsBeforeAsync`. Only then is a
    /// command left running when the agent goes on taken for one.
    pub backgrounds_commands: bool,
}

/// The ACP server's command line, and the settings to apply once its
/// session is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Startup {
    pub program: String,
    pub arguments: Vec<String>,
    pub settings: Settings,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub mode: Option<String>,
}

/// ACP's own sign-in signal, the `auth_required` error code.
pub fn auth_required(error: &Value) -> bool {
    error["code"].as_i64() == Some(rpc::AUTH_REQUIRED)
}

/// How a sign-in failure begins, which [`chat::requires_sign_in`]
/// recognises for an ACP provider.
pub const SIGN_IN_PREFIX: &str = chat::ACP_SIGN_IN_PREFIX;

/// Whether an error reply refuses the request because the login has no
/// usage allowance. ACP has no code for it, so this reads the agent's own
/// words for it - its message, its data's message, details, reason and code,
/// and an HTTP status - and nothing else the data carries, such as an echoed
/// request whose text could say anything. An allowance Muse merely has no
/// figures for is not this.
pub fn no_allowance(error: &Value) -> bool {
    let data = &error["data"];
    if [&data["reason"], &data["code"]]
        .iter()
        .filter_map(|value| value.as_str())
        .any(|value| value.trim().eq_ignore_ascii_case("no_allowance"))
        || [&data["status"], &data["statusCode"]]
            .iter()
            .any(|status| status.as_i64() == Some(402))
    {
        return true;
    }
    [&error["message"], &data["message"], &data["details"], data]
        .iter()
        .filter_map(|value| value.as_str())
        .any(|words| {
            // Bounded: a detail can carry a whole response.
            let words = words
                .chars()
                .take(2048)
                .collect::<String>()
                .to_ascii_lowercase();
            words.contains("402 payment required")
                || words.contains("no subscription allowance")
                || words.contains("(no_allowance)")
        })
}

pub fn create(
    profile: &'static Profile,
    launch: &Launch,
) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    let (spec, adapter) = prepare(profile, launch)?;
    Ok((spec, Box::new(adapter)))
}

fn prepare(profile: &'static Profile, launch: &Launch) -> Result<(ProcessSpec, Acp), String> {
    let startup = (profile.launch)(launch)?;
    let directory = &launch.recipe.working_directory;
    if !directory.is_absolute() {
        return Err(format!(
            "{} needs an absolute working directory for its conversation.",
            profile.label
        ));
    }
    let cwd = directory
        .to_str()
        .ok_or_else(|| {
            format!(
                "{} cannot work in {}: its path is not text.",
                profile.label,
                directory.display()
            )
        })?
        .to_owned();
    if let Some(id) = &launch.session_id
        && (id.is_empty() || id.len() > MAX_SESSION_ID || id.contains(char::is_control))
    {
        return Err(format!(
            "{} cannot resume a conversation without its exact session ID.",
            profile.label
        ));
    }
    let mut spec = ProcessSpec::from(&launch.recipe);
    spec.program = startup.program;
    spec.arguments = startup.arguments;
    // An agent's launcher or shell startup may print to stdout; such a line
    // is skipped unless it hides a protocol frame.
    spec.stdout_noise = Some(rpc::embedded_frame);
    spec.followed = true;
    let settings = startup.settings;
    Ok((
        spec,
        Acp {
            profile,
            cwd,
            resume: launch.session_id.clone(),
            settings: [
                settings.model.map(Setting::Model),
                settings.reasoning.map(Setting::Reasoning),
                settings.mode.map(Setting::Mode),
            ]
            .into_iter()
            .flatten()
            .collect(),
            ..Acp::new(profile)
        },
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Initializing,
    /// `session/new` or `session/load` is outstanding.
    Opening,
    /// The launch's settings are being applied.
    Configuring,
    Ready,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Setting {
    Model(String),
    Reasoning(String),
    Mode(String),
}

/// What a request cagents sent is waiting to hear.
#[derive(Debug)]
enum Pending {
    Initialize,
    Open,
    ListTitle { pages_left: u8 },
    Setting(Setting),
    Prompt,
}

/// The prompt in flight, from its request until its reply.
struct Turn {
    /// The turn's `session/prompt` requests still unanswered, oldest first:
    /// the one that started it, and each message steered into it since. The
    /// agent answers an earlier one when a later one takes over the turn,
    /// which ends with the reply to the last.
    requests: Vec<u64>,
    key: String,
    /// `session/cancel` has been sent; the reply decides how the turn ended.
    cancelling: bool,
    run: Option<crate::model::RunId>,
    /// The agent has said all it will until the commands it left running in
    /// the background finish.
    on_background: bool,
}

/// Why a message cannot steer a turn that answers a correlated run: it
/// would change the answer the run's caller is waiting for.
const RUN_TURN_REFUSAL: &str = "This turn is answering a message that waits for its reply, and \
     steering it would change that reply. Queue the message for the next turn.";

/// A message written during a turn, held for the next one.
struct Queued {
    message: chat::QueuedMessage,
    blocks: Vec<Value>,
    /// The message as the transcript shows it once it is sent.
    items: Vec<Item>,
    /// Its blocks' serialized size, which the queue's bound counts.
    bytes: usize,
}

/// A text stream being written into one item.
struct Segment {
    role: Role,
    thinking: bool,
    message: Option<String>,
    id: String,
}

/// An open `session/request_permission`.
struct Permission {
    /// The JSON-RPC identifier to answer, exactly as the agent sent it.
    rpc: Value,
    /// Each option's identifier and its kind, which alone says whether
    /// choosing it refuses the action.
    options: Vec<(String, String)>,
    /// The tool call it asks about.
    call: String,
}

struct Acp {
    profile: &'static Profile,
    cwd: String,
    resume: Option<String>,
    stage: Stage,
    next_request: u64,
    pending: HashMap<u64, Pending>,
    session: Option<String>,
    list_titles: bool,
    title: Option<String>,
    images: bool,
    config: config::Config,
    /// Settings waiting to be sent; only one is in flight at a time, since
    /// choosing a model can change the reasoning levels it offers.
    settings: VecDeque<Setting>,
    setting_in_flight: bool,
    commands: Vec<SlashCommand>,
    replay: Option<history::Replay>,
    calls: tools::Calls,
    segment: Option<Segment>,
    /// How many items each message identifier has opened, so a message the
    /// agent resumes after a tool call continues below it in a new item.
    segment_ids: HashMap<String, u32>,
    next_item: u64,
    turn: Option<Turn>,
    permissions: BTreeMap<String, Permission>,
    next_permission: u64,
    usage: chat::Usage,
    queued: Vec<Queued>,
    next_queued: u64,
    /// Names this adapter's queued messages apart from those of the one
    /// before it, whose queue went with its process: removing one of those
    /// cannot be answered as if it had been delivered.
    queue_tag: String,
}

impl Acp {
    fn new(profile: &'static Profile) -> Self {
        Self {
            profile,
            cwd: String::new(),
            resume: None,
            stage: Stage::Initializing,
            next_request: 0,
            pending: HashMap::new(),
            session: None,
            list_titles: false,
            title: None,
            images: false,
            config: config::Config::default(),
            settings: VecDeque::new(),
            setting_in_flight: false,
            commands: Vec::new(),
            replay: None,
            calls: tools::Calls::default(),
            segment: None,
            segment_ids: HashMap::new(),
            next_item: 0,
            turn: None,
            permissions: BTreeMap::new(),
            next_permission: 0,
            usage: chat::Usage::default(),
            queued: Vec::new(),
            next_queued: 0,
            queue_tag: uuid::Uuid::new_v4().simple().to_string()[..8].to_owned(),
        }
    }

    fn request(&mut self, method: &str, params: Value, pending: Pending) -> (u64, Value) {
        self.next_request += 1;
        let id = self.next_request;
        self.pending.insert(id, pending);
        (id, rpc::request(id, method, params))
    }

    fn item_id(&mut self) -> String {
        self.next_item += 1;
        format!("acp:{}", self.next_item)
    }

    fn session_id(&self) -> &str {
        self.session.as_deref().unwrap_or_default()
    }

    fn set_title(&mut self, title: &str, events: &mut Vec<Event>) {
        if self.title.as_deref() != Some(title) {
            self.title = Some(title.to_owned());
            events.push(Event::Session {
                id: self.session_id().to_owned(),
                title: self.title.clone(),
            });
        }
    }

    fn discard_title_request(&mut self) {
        self.pending
            .retain(|_, request| !matches!(request, Pending::ListTitle { .. }));
    }

    fn refresh_title(&mut self, pages: u8, step: &mut Step) {
        if self.list_titles {
            self.discard_title_request();
            self.list_title_page(None, pages, step);
        }
    }

    fn list_title_page(&mut self, cursor: Option<&str>, pages_left: u8, step: &mut Step) {
        let mut params = json!({"cwd": self.cwd});
        if let Some(cursor) = cursor {
            params["cursor"] = json!(cursor);
        }
        let (_, frame) = self.request("session/list", params, Pending::ListTitle { pages_left });
        step.writes.push(frame);
    }

    fn listed_title(&mut self, outcome: Result<&Value, &Value>, pages_left: u8, step: &mut Step) {
        let result = match outcome {
            Ok(result) => result,
            Err(error) => {
                if error["code"].as_i64() == Some(rpc::METHOD_NOT_FOUND) {
                    self.list_titles = false;
                }
                return;
            }
        };
        let own_session = result["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| entry["sessionId"].as_str() == self.session.as_deref());
        if let Some(entry) = own_session {
            if let Some(title) = title_text(&entry["title"]) {
                self.set_title(title, &mut step.events);
            }
        } else if pages_left > 1
            && let Some(cursor) = result["nextCursor"]
                .as_str()
                .filter(|cursor| !cursor.is_empty())
        {
            self.list_title_page(Some(cursor), pages_left - 1, step);
        }
    }

    /// The message a reader is shown for an error reply: its own words, or
    /// for a sign-in failure, what to do about it, or for a login with no
    /// allowance, the agent's reason, marked as one.
    fn failure(&self, error: &Value) -> String {
        if (self.profile.sign_in_required)(error) {
            format!(
                "{SIGN_IN_PREFIX}: sign in to {}, then try again. ({})",
                self.profile.label,
                rpc::error_message(error)
            )
        } else if no_allowance(error) {
            format!(
                "{}{}",
                chat::ACP_NO_ALLOWANCE_PREFIX,
                rpc::error_reason(error)
            )
        } else {
            rpc::error_message(error)
        }
    }

    fn fail(&mut self, message: String, step: &mut Step) {
        self.stage = Stage::Failed;
        step.events.push(Event::Error {
            message,
            fatal: true,
        });
    }

    fn capabilities(&self) -> Event {
        Event::Capabilities(Capabilities {
            steer: self.profile.steers,
            // A message queued during a turn waits here for the next one.
            queue: true,
            queue_remove: true,
            // One queued message can go ahead of the rest, or, held between
            // turns by a stop or a refusal, start the next turn.
            queue_start: true,
            queue_holds: true,
            // Sending what is queued now steers it into the running turn.
            send_queued_now: self.profile.steers,
            interrupt_stops_background: self.profile.cancel_stops_background,
            attachments: true,
            compact: self.compacts(),
            model_selection: self.config.model_selection(),
            reasoning_selection: self.config.reasoning_selection(),
            permission_modes: self.permission_modes(),
            skill_reach: SkillReach::Leading,
            ..Capabilities::default()
        })
    }

    /// The modes the session offers, by what each means, in the profile's
    /// order; a mode the profile does not know follows in the agent's own.
    fn permission_modes(&self) -> Vec<String> {
        let mut modes = self.config.mode_ids();
        modes.sort_by_key(|mode| {
            self.profile
                .modes
                .iter()
                .position(|(own, _)| own == mode)
                .unwrap_or(usize::MAX)
        });
        modes
            .iter()
            .map(|mode| self.profile.mode_meaning(mode).to_owned())
            .collect()
    }

    fn compacts(&self) -> bool {
        self.commands
            .iter()
            .any(|command| command.name == "compact")
    }

    fn configuration_changed(&self, step: &mut Step) {
        step.events
            .extend(self.config.events().into_iter().map(|event| match event {
                Event::PermissionMode(Some(mode)) => {
                    Event::PermissionMode(Some(self.profile.mode_meaning(&mode).to_owned()))
                }
                event => event,
            }));
        if self.stage == Stage::Ready {
            step.events.push(self.capabilities());
        }
    }

    /// Sends the next queued setting, if none is in flight. Returns whether
    /// one is now outstanding.
    fn send_setting(&mut self, step: &mut Step) -> bool {
        while !self.setting_in_flight {
            let Some(mut setting) = self.settings.pop_front() else {
                return false;
            };
            // A terminal's model can name a reasoning level too, as agy's
            // `gemini-3.8-flash-low` does, where the session offers the model
            // and its level as two options.
            if let Setting::Model(model) = &setting
                && !self.config.offers_model(model)
                && let Some((base, level)) = self.config.split_model(model)
            {
                if !self
                    .settings
                    .iter()
                    .any(|queued| matches!(queued, Setting::Reasoning(_)))
                {
                    self.settings.push_front(Setting::Reasoning(level));
                }
                setting = Setting::Model(base);
            }
            let (setter, value, offered, what) = match &setting {
                Setting::Model(model) => (
                    self.config.model_setter(),
                    model,
                    self.config.offers_model(model),
                    "model",
                ),
                Setting::Reasoning(level) => (
                    self.config
                        .reasoning_option()
                        .map(|id| config::Setter::Option(id.to_owned())),
                    level,
                    self.config.offers_reasoning(level),
                    "reasoning level",
                ),
                Setting::Mode(mode) => (
                    self.config.mode_setter(),
                    mode,
                    self.config.offers_mode(mode),
                    "mode",
                ),
            };
            let Some(setter) = setter.filter(|_| offered) else {
                step.events.push(Event::Error {
                    message: format!(
                        "{} does not offer the {what} {value:?} in this conversation.",
                        self.profile.label
                    ),
                    fatal: false,
                });
                continue;
            };
            let session = self.session_id().to_owned();
            let (method, params) = match setter {
                config::Setter::Option(id) => (
                    "session/set_config_option",
                    json!({"sessionId": session, "configId": id, "value": value}),
                ),
                config::Setter::Model => (
                    "session/set_model",
                    json!({"sessionId": session, "modelId": value}),
                ),
                config::Setter::Mode => (
                    "session/set_mode",
                    json!({"sessionId": session, "modeId": value}),
                ),
            };
            let (_, frame) = self.request(method, params, Pending::Setting(setting));
            step.writes.push(frame);
            self.setting_in_flight = true;
        }
        true
    }

    fn become_ready(&mut self, step: &mut Step) {
        self.stage = Stage::Ready;
        step.events.push(self.capabilities());
        step.events.push(Event::Ready(true));
        step.events.push(Event::Status {
            busy: false,
            detail: "Ready".into(),
        });
    }

    /// Refuses a new turn before the conversation is ready or while one is
    /// still running, before anything is prepared for it.
    fn accepts_turn(&self) -> Result<(), String> {
        if self.stage != Stage::Ready {
            return Err(format!(
                "{} is still starting its conversation.",
                self.profile.label
            ));
        }
        if self.turn.is_some() {
            return Err(format!(
                "{} is still working on the last message; wait for it or stop it.",
                self.profile.label
            ));
        }
        Ok(())
    }

    fn start_turn(&mut self, blocks: Vec<Value>, items: Vec<Item>) -> Result<Step, String> {
        self.accepts_turn()?;
        let mut step = Step::default();
        self.begin_turn(blocks, items, &mut step);
        Ok(step)
    }

    fn begin_turn(&mut self, blocks: Vec<Value>, items: Vec<Item>, step: &mut Step) {
        let params = json!({"sessionId": self.session_id(), "prompt": blocks});
        let (id, frame) = self.request("session/prompt", params, Pending::Prompt);
        step.writes.push(frame);
        self.segment = None;
        self.turn = Some(Turn {
            requests: vec![id],
            key: format!("acp-prompt:{}:{id}", self.session_id()),
            cancelling: false,
            run: None,
            on_background: false,
        });
        step.events.extend(items.into_iter().map(Event::Item));
        step.events.push(Event::TurnStarted);
        step.events.push(Event::Status {
            busy: true,
            detail: "Working".into(),
        });
    }

    /// Sends `blocks` into the running turn, which the agent goes on with;
    /// see [`Profile::steers`].
    fn steer(&mut self, blocks: Vec<Value>, items: Vec<Item>, step: &mut Step) {
        let params = json!({"sessionId": self.session_id(), "prompt": blocks});
        let (id, frame) = self.request("session/prompt", params, Pending::Prompt);
        step.writes.push(frame);
        self.segment = None;
        step.events.extend(items.into_iter().map(Event::Item));
        if let Some(turn) = &mut self.turn {
            turn.requests.push(id);
        }
        self.at_work(&mut step.events);
    }

    /// The agent is at work again after waiting on its background commands.
    fn at_work(&mut self, events: &mut Vec<Event>) {
        if let Some(turn) = self.turn.as_mut().filter(|turn| turn.on_background) {
            turn.on_background = false;
            // A turn being stopped already says so.
            if turn.cancelling {
                return;
            }
            events.push(Event::Status {
                busy: true,
                detail: "Working".into(),
            });
        }
    }

    /// The agent went on while its commands ran: they run in the background.
    fn went_on(&mut self) {
        if self.profile.backgrounds_commands {
            self.calls.background_running();
        }
    }

    /// Whether the turn now waits only on commands the agent left running
    /// in the background, having said all it will until they finish: an
    /// agent reports its usage as each model response ends. Published once,
    /// until the agent is at work again.
    fn awaits_background(&mut self, events: &mut Vec<Event>) {
        if !self.profile.backgrounds_commands {
            return;
        }
        let waits = self.permissions.is_empty() && self.calls.only_background_live();
        if let Some(turn) = self.turn.as_mut().filter(|turn| !turn.cancelling)
            && waits
            && !turn.on_background
        {
            turn.on_background = true;
            events.push(Event::Status {
                busy: true,
                detail: chat::WAITING_ON_BACKGROUND_WORK.into(),
            });
        }
    }

    /// Joins `blocks` to the queue for the next turn.
    fn queue(
        &mut self,
        text: String,
        attachments: Vec<chat::Attachment>,
        blocks: Vec<Value>,
        items: Vec<Item>,
    ) -> Result<Step, String> {
        let bytes = blocks
            .iter()
            .map(|block| serde_json::to_vec(block).map_or(0, |encoded| encoded.len()))
            .sum::<usize>();
        let held = self.queued.iter().map(|queued| queued.bytes).sum::<usize>();
        if self.queued.len() >= MAX_QUEUED || held + bytes > MAX_QUEUED_BYTES {
            return Err(format!(
                "{} has as many messages waiting as it can hold; wait for them to go, or remove one.",
                self.profile.label
            ));
        }
        self.next_queued += 1;
        let mut message = chat::QueuedMessage::new(self.queued_id(self.next_queued), text);
        message.attachments = attachments;
        self.queued.push(Queued {
            message,
            blocks,
            items,
            bytes,
        });
        let mut step = Step::default();
        self.queue_changed(&mut step);
        Ok(step)
    }

    fn queued_id(&self, number: u64) -> String {
        format!("acp-queued:{}:{number}", self.queue_tag)
    }

    /// Whether `id` names a message this adapter queued, whether or not it
    /// is still queued.
    fn queued_here(&self, id: &str) -> bool {
        id.strip_prefix("acp-queued:")
            .and_then(|rest| rest.strip_prefix(self.queue_tag.as_str()))
            .and_then(|rest| rest.strip_prefix(':'))
            .and_then(|number| number.parse::<u64>().ok())
            .is_some_and(|number| (1..=self.next_queued).contains(&number))
    }

    fn queue_changed(&self, step: &mut Step) {
        step.events.push(Event::QueueChanged(
            self.queued
                .iter()
                .map(|queued| queued.message.clone())
                .collect(),
        ));
    }

    /// Everything queued, as one prompt: each message's blocks in the order
    /// they were written, and each message shown as it was written.
    fn take_queued(&mut self, step: &mut Step) -> Option<(Vec<Value>, Vec<Item>)> {
        if self.queued.is_empty() {
            return None;
        }
        let (mut blocks, mut items) = (Vec::new(), Vec::new());
        for queued in std::mem::take(&mut self.queued) {
            blocks.extend(queued.blocks);
            items.extend(queued.items);
        }
        self.queue_changed(step);
        Some((blocks, items))
    }

    /// A reply to one of the turn's prompts: the last one's ends the turn,
    /// and an earlier one's only says that a later one took over.
    fn answered(&mut self, request: u64, outcome: Result<&Value, &Value>, step: &mut Step) {
        if let Ok(result) = outcome {
            // The unstable `PromptResponse.usage` is this prompt's own
            // tokens, so prompts add up: Antigravity 1.5.12's second turn of
            // a conversation reports only its own input, apart from what it
            // read from cache, as Claude's do. Within a steered turn each
            // reply counts only the model's responses made under its own
            // prompt: in 1.5.17, the first reply's 12386 tokens were the one
            // response before the steer, and the second's 52696 the four
            // after it, to the token.
            if let Some(usage) = result.get("usage").filter(|usage| usage.is_object()) {
                let count = |key: &str| usage[key].as_u64().unwrap_or(0);
                self.usage.input_tokens += count("inputTokens");
                self.usage.output_tokens += count("outputTokens");
                step.events.push(Event::Usage(self.usage.clone()));
            }
        }
        // A late reply to a prompt of a turn already ended says nothing of
        // the one running now.
        let Some(turn) = self
            .turn
            .as_mut()
            .filter(|turn| turn.requests.contains(&request))
        else {
            return;
        };
        turn.requests.retain(|pending| *pending != request);
        // Stopped, the turn is over once any of its prompts says so: an
        // agent owes the others no reply of their own.
        let stopped = turn.cancelling
            && match outcome {
                Ok(result) => result["stopReason"] == "cancelled",
                Err(error) => error["code"].as_i64() == Some(rpc::REQUEST_CANCELLED),
            };
        if turn.requests.is_empty() || stopped {
            return self.end_turn(outcome, step);
        }
        // A later message took the turn over. Only an earlier prompt that
        // failed has anything to say; one stopped by the reader's Stop is
        // what they asked for, and the turn's last reply says how it ended.
        let cancelling = turn.cancelling;
        if let Err(error) = outcome
            && !(cancelling && error["code"].as_i64() == Some(rpc::REQUEST_CANCELLED))
        {
            step.events.push(Event::Error {
                message: self.failure(error),
                fatal: false,
            });
        }
    }

    fn end_turn(&mut self, outcome: Result<&Value, &Value>, step: &mut Step) {
        let Some(turn) = self.turn.take() else {
            return;
        };
        self.segment = None;
        let (kind, notice) = match outcome {
            Ok(result) => match result["stopReason"].as_str() {
                Some("end_turn") => (Kind::Succeeded, None),
                Some("cancelled") => (Kind::Cancelled, None),
                Some("max_tokens") => (
                    Kind::Failed,
                    Some(format!(
                        "{} stopped: it reached its output limit.",
                        self.profile.label
                    )),
                ),
                Some("max_turn_requests") => (
                    Kind::Failed,
                    Some(format!(
                        "{} stopped: the turn reached its limit of model requests.",
                        self.profile.label
                    )),
                ),
                Some("refusal") => (
                    Kind::Failed,
                    Some(format!("{} declined to continue.", self.profile.label)),
                ),
                other => (
                    Kind::Failed,
                    Some(format!(
                        "{} ended the turn for a reason cagents does not know ({}).",
                        self.profile.label,
                        other.unwrap_or("none given")
                    )),
                ),
            },
            // A turn cancelled before the agent could answer is still the
            // cancellation the reader asked for.
            Err(error)
                if turn.cancelling && error["code"].as_i64() == Some(rpc::REQUEST_CANCELLED) =>
            {
                (Kind::Cancelled, None)
            }
            Err(error) => (Kind::Failed, Some(self.failure(error))),
        };
        for (id, content) in self.calls.stop_running() {
            step.events.push(Event::Item(tool_item(id, content)));
        }
        // An agent that ended its turn can no longer act on an answer.
        if !self.permissions.is_empty() {
            for (id, permission) in std::mem::take(&mut self.permissions) {
                step.writes.push(rpc::result(
                    &permission.rpc,
                    json!({"outcome": {"outcome": "cancelled"}}),
                ));
                step.events.push(Event::RequestResolved { id });
            }
            step.events.push(Event::Waiting(false));
        }
        if let Some(message) = notice {
            step.events.push(Event::Error {
                message,
                fatal: false,
            });
        }
        step.events.push(Event::TurnSettled);
        // What was queued starts the next turn in this same step, so the
        // conversation never looks idle to whatever would start one of its
        // own in between, such as a message from another session. After a
        // refusal to work at all, or the reader's Stop, it waits for the
        // reader instead: their next message, Send now or starting one
        // queued message.
        let refused = outcome
            .is_err_and(|error| (self.profile.sign_in_required)(error) || no_allowance(error));
        let next = if refused || kind == Kind::Cancelled {
            None
        } else {
            self.take_queued(step)
        };
        if next.is_none() {
            step.events.push(Event::Status {
                busy: false,
                detail: "Ready".into(),
            });
        }
        if let Some(run) = turn.run {
            use crate::agent_execution::ProviderEvent;
            step.events.push(Event::Execution {
                run,
                event: ProviderEvent::Settled,
            });
            step.events.push(Event::Execution {
                run,
                event: ProviderEvent::Finished { outcome: kind },
            });
        }
        step.events.push(Event::TurnFinished {
            key: Some(turn.key),
            outcome: kind,
        });
        if let Some((blocks, items)) = next {
            self.begin_turn(blocks, items, step);
        }
        self.refresh_title(MAX_TURN_TITLE_LIST_PAGES, step);
    }

    fn respond(&mut self, id: u64, outcome: Result<&Value, &Value>, step: &mut Step) {
        let Some(pending) = self.pending.remove(&id) else {
            return;
        };
        match pending {
            Pending::Initialize => match outcome {
                Ok(result) => self.initialized(result, step),
                Err(error) => {
                    let message = self.failure(error);
                    self.fail(message, step);
                }
            },
            Pending::Open => match outcome {
                Ok(result) => self.opened(result, step),
                Err(error) => {
                    let message = match &self.resume {
                        Some(_) if !(self.profile.sign_in_required)(error) => format!(
                            "{} could not resume this conversation: {}",
                            self.profile.label,
                            self.failure(error)
                        ),
                        _ => self.failure(error),
                    };
                    self.replay = None;
                    self.fail(message, step);
                }
            },
            Pending::ListTitle { pages_left } => self.listed_title(outcome, pages_left, step),
            Pending::Setting(setting) => {
                self.setting_in_flight = false;
                match outcome {
                    Ok(result) => {
                        let options = result["configOptions"].as_array();
                        if let Some(options) = options {
                            self.config.learn_options(options);
                            self.config.retain_models(self.profile.offers_model);
                        }
                        match &setting {
                            Setting::Model(model) => {
                                self.config.model_changed(model);
                                // Each model offers its own reasoning levels,
                                // as Muse's do. A level queued with the model
                                // that the new model's options leave out is
                                // dropped rather than sent and refused.
                                if options.is_some()
                                    && let Some(Setting::Reasoning(level)) = self.settings.front()
                                    && !self.config.offers_reasoning(level)
                                {
                                    self.settings.pop_front();
                                }
                            }
                            Setting::Mode(mode) => self.config.mode_changed(mode),
                            Setting::Reasoning(_) => {}
                        }
                        self.configuration_changed(step);
                    }
                    Err(error) => {
                        let (what, value) = match &setting {
                            Setting::Model(model) => ("model", model),
                            Setting::Reasoning(level) => ("reasoning level", level),
                            Setting::Mode(mode) => ("mode", mode),
                        };
                        step.events.push(Event::Error {
                            message: format!(
                                "{} did not change its {what} to {value:?}: {}",
                                self.profile.label,
                                self.failure(error)
                            ),
                            fatal: false,
                        });
                    }
                }
                if !self.send_setting(step) && self.stage == Stage::Configuring {
                    self.become_ready(step);
                }
            }
            Pending::Prompt => self.answered(id, outcome, step),
        }
    }

    fn initialized(&mut self, result: &Value, step: &mut Step) {
        let version = result["protocolVersion"].as_u64();
        if version != Some(PROTOCOL_VERSION) {
            let version =
                version.map_or_else(|| "an unknown version".into(), |v| format!("version {v}"));
            return self.fail(
                format!(
                    "{} speaks Agent Client Protocol {version}; cagents supports version {PROTOCOL_VERSION}.",
                    self.profile.label
                ),
                step,
            );
        }
        let capabilities = &result["agentCapabilities"];
        self.images = capabilities["promptCapabilities"]["image"].as_bool() == Some(true);
        self.list_titles = capabilities["sessionCapabilities"]["list"].is_object();
        self.stage = Stage::Opening;
        let cwd = self.cwd.clone();
        match self.resume.clone() {
            Some(id) => {
                if capabilities["loadSession"].as_bool() != Some(true) {
                    return self.fail(
                        format!(
                            "This version of {} cannot resume a conversation in cagents.",
                            self.profile.label
                        ),
                        step,
                    );
                }
                self.session = Some(id.clone());
                self.replay = Some(history::Replay::default());
                step.events.push(Event::HistoryLoading(true));
                let (_, frame) = self.request(
                    "session/load",
                    json!({"sessionId": id, "cwd": cwd, "mcpServers": []}),
                    Pending::Open,
                );
                step.writes.push(frame);
            }
            None => {
                let (_, frame) = self.request(
                    "session/new",
                    json!({"cwd": cwd, "mcpServers": []}),
                    Pending::Open,
                );
                step.writes.push(frame);
            }
        }
    }

    fn opened(&mut self, result: &Value, step: &mut Step) {
        if self.resume.is_none() {
            let Some(id) = result["sessionId"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= MAX_SESSION_ID)
            else {
                return self.fail(
                    format!("{} did not name its new conversation.", self.profile.label),
                    step,
                );
            };
            self.session = Some(id.to_owned());
        }
        if let Some(mut replay) = self.replay.take() {
            // A step the saved conversation left running ended with the
            // process that ran it; it is not running now.
            for (id, content) in self.calls.stop_running() {
                replay.absorb(Event::Item(tool_item(id, content)));
            }
            let (items, opening_title) = replay.finish();
            if let Some(title) = opening_title {
                step.events.push(Event::OpeningTitle(title));
            }
            step.events.push(Event::History {
                items,
                has_earlier: false,
            });
        }
        self.segment = None;
        step.events.push(Event::Session {
            id: self.session_id().to_owned(),
            title: None,
        });
        if self.resume.is_none() {
            step.events.push(Event::OpeningHistoryComplete);
        }
        self.config.learn(result);
        self.config.retain_models(self.profile.offers_model);
        self.configuration_changed(step);
        self.stage = Stage::Configuring;
        if !self.send_setting(step) {
            self.become_ready(step);
        }
        // Session load replays messages, but an agent may only publish a
        // saved conversation's title in session/list.
        if self.resume.is_some() {
            self.refresh_title(MAX_SAVED_TITLE_LIST_PAGES, step);
        }
    }

    fn permission(&mut self, rpc: &Value, params: &Value, step: &mut Step) {
        let call = &params["toolCall"];
        let call_id = call["toolCallId"].as_str().unwrap_or_default().to_owned();
        if let Some((id, content)) = self.calls.merge(call) {
            self.segment = None;
            step.events.push(Event::Item(tool_item(id, content)));
        }
        self.at_work(&mut step.events);
        let options = params["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|option| {
                Some((
                    option["optionId"].as_str()?.to_owned(),
                    option["name"].as_str().unwrap_or_default().to_owned(),
                    option["kind"].as_str().unwrap_or_default().to_owned(),
                ))
            })
            .take(16)
            .collect::<Vec<_>>();
        // A request that cannot be answered, that arrives after the reader
        // stopped the turn, or beyond what a reader could work through, is
        // cancelled as the protocol requires.
        let stopping = self.turn.as_ref().is_some_and(|turn| turn.cancelling);
        if options.is_empty() || stopping || self.permissions.len() >= MAX_PERMISSIONS {
            step.writes.push(rpc::result(
                rpc,
                json!({"outcome": {"outcome": "cancelled"}}),
            ));
            self.calls.declined(&call_id);
            return;
        }
        self.next_permission += 1;
        let id = format!("acp-permission:{}", self.next_permission);
        let actions = options
            .iter()
            .map(|(option, name, kind)| Choice {
                id: option.clone(),
                label: if name.is_empty() {
                    option.clone()
                } else {
                    name.clone()
                },
                description: match kind.as_str() {
                    "allow_once" => "Allow this time",
                    "allow_always" => "Allow from now on",
                    "reject_once" => "Refuse this time",
                    "reject_always" => "Refuse from now on",
                    _ => "",
                }
                .into(),
            })
            .collect();
        step.events.push(Event::Request(Request {
            kind: RequestKind::Approval,
            id: id.clone(),
            title: self
                .calls
                .title(&call_id)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{} asks to continue", self.profile.label)),
            description: self.calls.summary(&call_id),
            fields: Vec::new(),
            actions,
        }));
        step.events.push(Event::Waiting(true));
        self.permissions.insert(
            id,
            Permission {
                rpc: rpc.clone(),
                options: options
                    .into_iter()
                    .map(|(option, _, kind)| (option, kind))
                    .collect(),
                call: call_id,
            },
        );
    }

    /// The agent no longer wants an answer to an approval it asked for.
    fn withdrawn(&mut self, rpc: &Value, step: &mut Step) {
        let Some(id) = self
            .permissions
            .iter()
            .find(|(_, permission)| permission.rpc == *rpc)
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        self.permissions.remove(&id);
        step.writes
            .push(rpc::error(rpc, rpc::REQUEST_CANCELLED, "Request cancelled"));
        step.events.push(Event::RequestResolved { id });
        if self.permissions.is_empty() {
            step.events.push(Event::Waiting(false));
        }
    }

    fn update(&mut self, params: &Value, step: &mut Step) {
        if params["sessionId"].as_str() != self.session.as_deref() || self.session.is_none() {
            return;
        }
        let update = &params["update"];
        let mut events = Vec::new();
        // What the agent does live, rather than replays, says whether it is
        // at work or waits on its background commands.
        let live = self.turn.is_some() && self.replay.is_none();
        match update["sessionUpdate"].as_str().unwrap_or_default() {
            kind @ ("user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk") => {
                let role = if kind == "user_message_chunk" {
                    Role::User
                } else {
                    Role::Assistant
                };
                // The reader's own message is shown when it is sent; an agent
                // that echoes it during the turn would show it twice.
                if role == Role::User && live {
                    return;
                }
                if live {
                    self.went_on();
                    self.at_work(&mut events);
                }
                self.chunk(role, kind == "agent_thought_chunk", update, &mut events);
            }
            "tool_call" | "tool_call_update" => {
                if let Some((id, content)) = self.calls.merge(update) {
                    self.segment = None;
                    events.push(Event::Item(tool_item(id, content)));
                }
                // A background command's own progress is not the agent at
                // work; its end, or any other step, is.
                if live && !self.calls.only_background_live() {
                    self.at_work(&mut events);
                }
            }
            "plan" => events.push(Event::Plan {
                steps: update["entries"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(MAX_PLAN_STEPS)
                    .filter_map(|entry| {
                        Some(PlanStep {
                            text: entry["content"].as_str()?.to_owned(),
                            status: match entry["status"].as_str() {
                                Some("in_progress") => PlanState::Active,
                                Some("completed") => PlanState::Done,
                                _ => PlanState::Pending,
                            },
                        })
                    })
                    .collect(),
            }),
            "available_commands_update" => {
                self.commands = update["availableCommands"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(MAX_COMMANDS)
                    .filter_map(|command| {
                        Some(SlashCommand {
                            name: command["name"].as_str()?.trim_start_matches('/').to_owned(),
                            description: command["description"]
                                .as_str()
                                .unwrap_or_default()
                                .to_owned(),
                        })
                    })
                    .collect();
                events.push(Event::Commands(self.commands.clone()));
                if self.stage == Stage::Ready {
                    events.push(self.capabilities());
                }
            }
            "current_mode_update" => {
                if let Some(mode) = update["currentModeId"].as_str() {
                    self.config.mode_changed(mode);
                    events.push(Event::PermissionMode(Some(
                        self.profile.mode_meaning(mode).to_owned(),
                    )));
                }
            }
            "config_option_update" => {
                if let Some(options) = update["configOptions"].as_array() {
                    self.config.learn_options(options);
                    self.config.retain_models(self.profile.offers_model);
                    let mut changed = Step::default();
                    self.configuration_changed(&mut changed);
                    events.extend(changed.events);
                }
            }
            "session_info_update" => {
                if let Some(title) = title_text(&update["title"]) {
                    // An agent that publishes title changes live needs no
                    // listing, and an outstanding list may be older.
                    self.list_titles = false;
                    self.discard_title_request();
                    self.set_title(title, &mut events);
                }
            }
            "usage_update" => {
                if let (Some(used), Some(size)) = (update["used"].as_u64(), update["size"].as_u64())
                {
                    self.usage.context_tokens = used;
                    self.usage.context_limit = size;
                    if update["cost"]["currency"] == "USD"
                        && let Some(amount) = update["cost"]["amount"]
                            .as_f64()
                            .filter(|amount| amount.is_finite() && *amount >= 0.0)
                    {
                        self.usage.cost_microusd = Some((amount * 1_000_000.0) as u64);
                    }
                    events.push(Event::Usage(self.usage.clone()));
                }
                // A model response ended: what it left running runs on in
                // the background, and may be all the turn waits for.
                if live {
                    self.went_on();
                    self.awaits_background(&mut events);
                }
            }
            // A kind from a later protocol revision, or an agent's own
            // extension, says nothing this client can show.
            _ => {}
        }
        match &mut self.replay {
            Some(replay) => step
                .events
                .extend(events.into_iter().filter_map(|event| replay.absorb(event))),
            None => step.events.extend(events),
        }
    }

    fn chunk(&mut self, role: Role, thinking: bool, update: &Value, events: &mut Vec<Event>) {
        let content = &update["content"];
        let delta = match content["type"].as_str() {
            Some("text") => content["text"].as_str().map(str::to_owned),
            Some("resource_link") => content["uri"].as_str().map(|uri| {
                let path = prompt::uri_path(uri).unwrap_or_else(|| uri.to_owned());
                match role {
                    Role::User => format!("\nAttached file: {path}"),
                    _ => path,
                }
            }),
            Some("resource") => content["resource"]["text"].as_str().map(str::to_owned),
            Some("image") => {
                let (Some(mime), Some(data)) =
                    (content["mimeType"].as_str(), content["data"].as_str())
                else {
                    return;
                };
                let path = format!("data:{mime};base64,{data}");
                if path.len() > chat::MAX_INLINE_IMAGE_URL_BYTES || !mime.starts_with("image/") {
                    return;
                }
                // An image interrupts an agent's text, but belongs to the
                // reader's message it came with.
                if role != Role::User {
                    self.segment = None;
                }
                let id = self.item_id();
                events.push(Event::Item(Item {
                    started_at: None,
                    id,
                    role,
                    content: ItemContent::Image {
                        path,
                        description: "Image".into(),
                        reference: None,
                    },
                    at: None,
                }));
                return;
            }
            _ => None,
        };
        let Some(delta) = delta.filter(|delta| !delta.is_empty()) else {
            return;
        };
        let message = update["messageId"].as_str().map(str::to_owned);
        let continues = self.segment.as_ref().is_some_and(|segment| {
            segment.role == role
                && segment.thinking == thinking
                && (message.is_none() || segment.message == message)
        });
        if !continues {
            let base = message.as_ref().map(|message| {
                format!(
                    "acp-{}:{message}",
                    match (role, thinking) {
                        (Role::User, _) => "user",
                        (_, true) => "thought",
                        _ => "message",
                    }
                )
            });
            let id = match base {
                Some(base)
                    if self.segment_ids.len() < MAX_SEGMENT_IDS
                        || self.segment_ids.contains_key(&base) =>
                {
                    let opened = self.segment_ids.entry(base.clone()).or_default();
                    *opened += 1;
                    match *opened {
                        1 => base,
                        n => format!("{base}:{n}"),
                    }
                }
                // Past the bound, a message is told apart by its order alone.
                _ => self.item_id(),
            };
            self.segment = Some(Segment {
                role,
                thinking,
                message,
                id,
            });
        }
        let id = self.segment.as_ref().map(|segment| segment.id.clone());
        events.push(Event::TextDelta {
            id: id.unwrap_or_default(),
            role,
            thinking,
            delta,
        });
    }
}

impl Profile {
    /// The cagents mode the agent's own mode `mode` means.
    pub fn mode_meaning<'a>(&self, mode: &'a str) -> &'a str {
        self.modes
            .iter()
            .find(|(own, _)| *own == mode)
            .map_or(mode, |(_, meaning)| *meaning)
    }

    /// The agent's own mode for the cagents mode `meaning`.
    pub fn own_mode<'a>(&self, meaning: &'a str) -> &'a str {
        self.modes
            .iter()
            .find(|(_, candidate)| *candidate == meaning)
            .map_or(meaning, |(own, _)| *own)
    }
}

/// Names `id` as the conversation `arguments` resume, in place of any other
/// they named, so the terminal command and the native conversation agree.
/// The profile's `launch` takes the conversation from the launch's session
/// ID, never from these arguments.
pub fn align_resume_identity(
    provider: crate::agent::Provider,
    arguments: &mut Vec<String>,
    id: String,
) {
    let mut kept = Vec::with_capacity(arguments.len() + 2);
    let mut given = std::mem::take(arguments).into_iter();
    while let Some(argument) = given.next() {
        let (flag, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), false), |(flag, _)| (flag, true));
        if provider.is_identity_option(flag) {
            if !inline {
                given.next();
            }
        } else if provider.is_latest_selector(flag) {
            // The conversation is named, never the most recent one.
        } else if provider.is_value_option(flag) && !inline {
            // An option's value is never mistaken for an option.
            let value = given.next();
            kept.push(argument);
            kept.extend(value);
        } else {
            kept.push(argument);
        }
    }
    provider.append_resume(&mut kept, id);
    *arguments = kept;
}

fn tool_item(id: String, content: ItemContent) -> Item {
    Item {
        started_at: None,
        id,
        role: Role::Assistant,
        content,
        at: None,
    }
}

impl Adapter for Acp {
    fn initialize(&mut self) -> Result<Step, String> {
        let (_, frame) = self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                // The agent works with its own tools; it asks cagents only
                // to approve what its permission mode leaves to the reader.
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                },
                "clientInfo": {
                    "name": "cagents",
                    "title": "cagents",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
            Pending::Initialize,
        );
        Ok(Step {
            writes: vec![frame],
            events: vec![
                Event::Status {
                    busy: false,
                    detail: "Starting".into(),
                },
                // A new process starts with nothing queued: what an earlier
                // one held went with it.
                Event::QueueChanged(Vec::new()),
            ],
            ..Step::default()
        })
    }

    fn initialization_pending(&self) -> bool {
        !matches!(self.stage, Stage::Ready | Stage::Failed)
    }

    fn outcome_pending(&self) -> bool {
        self.turn.as_ref().is_some_and(|turn| turn.cancelling)
    }

    fn answer_declines(&self, request_id: &str, action: &str) -> bool {
        // The same reading of the action as answering it: an offered option
        // by its kind, and otherwise the request cancelled.
        self.permissions.get(request_id).is_some_and(|permission| {
            match permission
                .options
                .iter()
                .find(|(option, _)| option == action)
            {
                Some((_, kind)) => kind.starts_with("reject"),
                None => action == "cancel",
            }
        })
    }

    fn command_for_run(
        &mut self,
        run: crate::model::RunId,
        command: Command,
    ) -> Result<Step, String> {
        if !matches!(
            &command,
            Command::Prompt {
                mode: PromptMode::Send,
                ..
            }
        ) {
            return Err(format!(
                "A correlated {} run requires one idle direct prompt",
                self.profile.label
            ));
        }
        // A prompt sent now takes what the reader's queue holds with it;
        // the run must not answer for messages the reader never sent it.
        if !self.queued.is_empty() {
            return Err(format!(
                "{} has the reader's queued messages to send first",
                self.profile.label
            ));
        }
        let mut step = self.command(command)?;
        let turn = self
            .turn
            .as_mut()
            .ok_or("The prompt did not start a turn")?;
        turn.run = Some(run);
        step.events.insert(
            0,
            Event::Execution {
                run,
                event: crate::agent_execution::ProviderEvent::Started {
                    key: turn.key.clone(),
                },
            },
        );
        Ok(step)
    }

    fn command(&mut self, command: Command) -> Result<Step, String> {
        let label = self.profile.label;
        match command {
            Command::Prompt {
                text,
                attachments,
                mode,
            } => {
                if self.stage != Stage::Ready {
                    self.accepts_turn()?;
                }
                // A message written for a running turn that has just ended
                // starts the next one.
                let mode = match (&self.turn, mode) {
                    (None, _) => PromptMode::Send,
                    (Some(turn), PromptMode::Steer) if turn.cancelling => {
                        return Err(format!(
                            "{label} is stopping this turn; send the message once it has stopped."
                        ));
                    }
                    (Some(_), PromptMode::Steer) if !self.profile.steers => {
                        return Err(format!(
                            "{label} cannot take a message mid-turn; queue it for the next turn."
                        ));
                    }
                    (Some(turn), PromptMode::Steer) if turn.run.is_some() => {
                        return Err(RUN_TURN_REFUSAL.into());
                    }
                    (Some(_), mode) => mode,
                };
                if mode == PromptMode::Send {
                    self.accepts_turn()?;
                }
                let mut next = 0;
                let base = self.next_item;
                let prompt = prompt::build(
                    &text,
                    &attachments,
                    self.images,
                    self.profile.max_image_bytes,
                    label,
                    &mut || {
                        next += 1;
                        format!("acp:{}", base + next)
                    },
                )?;
                let step = match mode {
                    // What a stop or a refusal held in the queue was written
                    // first, so it goes first, in the same prompt.
                    PromptMode::Send => {
                        let mut step = Step::default();
                        let (mut blocks, mut items) =
                            self.take_queued(&mut step).unwrap_or_default();
                        blocks.extend(prompt.blocks);
                        items.extend(prompt.items);
                        self.begin_turn(blocks, items, &mut step);
                        step
                    }
                    PromptMode::Queue => {
                        self.queue(text, attachments, prompt.blocks, prompt.items)?
                    }
                    PromptMode::Steer => {
                        let mut step = Step::default();
                        self.steer(prompt.blocks, prompt.items, &mut step);
                        step
                    }
                };
                self.next_item += next;
                Ok(step)
            }
            Command::Slash { command } => {
                let text = command.trim();
                let text = if text.starts_with('/') {
                    text.to_owned()
                } else {
                    format!("/{text}")
                };
                let id = self.item_id();
                self.start_turn(
                    vec![json!({"type": "text", "text": text})],
                    vec![Item {
                        started_at: None,
                        id,
                        role: Role::User,
                        content: ItemContent::Text {
                            text,
                            thinking: false,
                            phase: chat::Phase::Final,
                        },
                        at: None,
                    }],
                )
            }
            Command::Compact if self.compacts() => self.command(Command::Slash {
                command: "/compact".into(),
            }),
            Command::Compact => Err(format!(
                "{label} does not offer a way to compact this conversation."
            )),
            Command::Interrupt => {
                let mut step = Step::default();
                let Some(turn) = self.turn.as_mut().filter(|turn| !turn.cancelling) else {
                    return Ok(step);
                };
                turn.cancelling = true;
                step.writes.push(rpc::notification(
                    "session/cancel",
                    json!({"sessionId": self.session.as_deref().unwrap_or_default()}),
                ));
                // The protocol requires every open approval to be answered
                // as cancelled once the turn is.
                if !self.permissions.is_empty() {
                    for (id, permission) in std::mem::take(&mut self.permissions) {
                        step.writes.push(rpc::result(
                            &permission.rpc,
                            json!({"outcome": {"outcome": "cancelled"}}),
                        ));
                        step.events.push(Event::RequestResolved { id });
                    }
                    step.events.push(Event::Waiting(false));
                }
                step.events.push(Event::Status {
                    busy: true,
                    detail: "Stopping".into(),
                });
                Ok(step)
            }
            Command::Answer {
                request_id,
                action,
                answers: _,
            } => {
                let permission = self
                    .permissions
                    .get(&request_id)
                    .ok_or("This approval request is no longer pending.")?;
                let outcome = if permission
                    .options
                    .iter()
                    .any(|(option, _)| *option == action)
                {
                    json!({"outcome": "selected", "optionId": action})
                } else if action == "cancel" {
                    json!({"outcome": "cancelled"})
                } else {
                    return Err(format!("{label} did not offer that choice."));
                };
                let declined = self.answer_declines(&request_id, &action);
                let permission = self.permissions.remove(&request_id).unwrap();
                // A call refused is over, whether or not the agent says so.
                if declined {
                    self.calls.declined(&permission.call);
                }
                let mut step = Step::default();
                step.writes
                    .push(rpc::result(&permission.rpc, json!({"outcome": outcome})));
                step.events.push(Event::RequestResolved { id: request_id });
                if self.permissions.is_empty() {
                    step.events.push(Event::Waiting(false));
                }
                Ok(step)
            }
            Command::SetModel { model, reasoning } => {
                if self.stage != Stage::Ready {
                    return Err(format!("{label} is still starting its conversation."));
                }
                if !self.config.offers_model(&model) {
                    return Err(format!("{label} does not offer the model {model:?}."));
                }
                self.settings.push_back(Setting::Model(model));
                if let Some(level) = reasoning {
                    self.settings.push_back(Setting::Reasoning(level));
                }
                let mut step = Step::default();
                self.send_setting(&mut step);
                Ok(step)
            }
            Command::SetPermissionMode { mode } => {
                if self.stage != Stage::Ready {
                    return Err(format!("{label} is still starting its conversation."));
                }
                let own = self.profile.own_mode(&mode).to_owned();
                if !self.config.offers_mode(&own) {
                    return Err(format!("{label} does not offer the mode {mode:?}."));
                }
                self.settings.push_back(Setting::Mode(own));
                let mut step = Step::default();
                self.send_setting(&mut step);
                Ok(step)
            }
            Command::Refresh => Ok(Step::default()),
            Command::DismissPermissionDenial { tool_use_id } => Ok(Step {
                events: vec![Event::PermissionDenialDismissed { tool_use_id }],
                ..Step::default()
            }),
            Command::LoadEarlier => Err(format!(
                "{label} loads a resumed conversation completely when it opens."
            )),
            Command::LoadLater => Err("Newer history is served by the transcript cache".into()),
            Command::ReadTextCopy { .. } => {
                Err("Text copies are served by the transcript cache".into())
            }
            Command::LoadFileChanges { .. } | Command::LoadFileChangePatches { .. } => {
                Err("File changes are served by the transcript cache".into())
            }
            Command::ResumeAfterLogin { .. } => Err(format!(
                "Signing in again is not yet available for {label} chats."
            )),
            Command::StopSubagent { .. } => Err(format!("{label} cannot stop a single subagent.")),
            Command::SetFastMode { .. } => Err(format!("{label} has no fast mode.")),
            Command::SetInterruptMode { .. } => Err(format!("{label} stops a turn only one way.")),
            Command::RemoveQueued { id } => {
                // Queued by an earlier process of the agent, it went with it.
                if !self.queued_here(&id) {
                    return Err(format!("That message is no longer queued with {label}."));
                }
                let mut step = Step::default();
                let position = self
                    .queued
                    .iter()
                    .position(|queued| queued.message.id == id);
                if let Some(position) = position {
                    self.queued.remove(position);
                }
                // One already sent has left the queue without being removed.
                step.events.push(Event::QueueRemoval(chat::QueueRemoval {
                    id,
                    removed: position.is_some(),
                }));
                self.queue_changed(&mut step);
                Ok(step)
            }
            Command::SendQueuedNow => {
                let mut step = Step::default();
                match &self.turn {
                    // Held by a stop or a refusal, what is queued starts a
                    // turn of its own.
                    None => {
                        self.accepts_turn()?;
                        if let Some((blocks, items)) = self.take_queued(&mut step) {
                            self.begin_turn(blocks, items, &mut step);
                        }
                    }
                    Some(turn) if turn.cancelling => {
                        return Err(format!(
                            "{label} is stopping this turn; send them once it has stopped."
                        ));
                    }
                    Some(_) if !self.profile.steers => {
                        return Err(format!("{label} cannot take a message mid-turn."));
                    }
                    Some(turn) if turn.run.is_some() => {
                        return Err(RUN_TURN_REFUSAL.into());
                    }
                    Some(_) => {
                        if let Some((blocks, items)) = self.take_queued(&mut step) {
                            self.steer(blocks, items, &mut step);
                        }
                    }
                }
                Ok(step)
            }
            Command::StartQueued { id } => {
                let position = self
                    .queued
                    .iter()
                    .position(|queued| queued.message.id == id)
                    .ok_or_else(|| format!("That message is no longer queued with {label}."))?;
                let between = self.turn.is_none();
                if between {
                    self.accepts_turn()?;
                }
                let mut step = Step::default();
                let queued = self.queued.remove(position);
                if between {
                    // Between turns, as a stop or a refusal leaves them, it
                    // starts the next turn, and the rest follow it.
                    self.queue_changed(&mut step);
                    self.begin_turn(queued.blocks, queued.items, &mut step);
                } else {
                    // During one, it goes first when the turn ends.
                    self.queued.insert(0, queued);
                    self.queue_changed(&mut step);
                }
                Ok(step)
            }
            Command::ListBranches | Command::Branch { .. } => {
                Err(format!("{label} cannot branch a conversation."))
            }
            Command::RefreshPermissions | Command::UpdatePermissionSettings { .. } => {
                Err("Permission rule settings are available for Claude sessions".into())
            }
        }
    }

    fn receive(&mut self, frame: Value) -> Result<Step, String> {
        let mut step = Step::default();
        let run = self
            .turn
            .as_ref()
            .and_then(|turn| turn.run)
            .filter(|_| self.replay.is_none());
        let mut scoped = false;
        match rpc::classify(&frame) {
            rpc::Frame::Request { id, method, params } => match method {
                "session/request_permission"
                    if self.session.is_some()
                        && params["sessionId"].as_str() == self.session.as_deref() =>
                {
                    scoped = true;
                    self.permission(id, params, &mut step);
                }
                "session/request_permission" => step.writes.push(rpc::error(
                    id,
                    rpc::INVALID_PARAMS,
                    "cagents has no such session",
                )),
                // Files and terminals were not offered, and anything newer
                // is not known; the agent hears so rather than waiting.
                _ => step.writes.push(rpc::error(
                    id,
                    rpc::METHOD_NOT_FOUND,
                    &format!("cagents does not provide {method}"),
                )),
            },
            rpc::Frame::Notification {
                method: "session/update",
                params,
            } => {
                scoped = true;
                self.update(params, &mut step);
            }
            rpc::Frame::Notification {
                method: "$/cancel_request",
                params,
            } => self.withdrawn(&params["requestId"], &mut step),
            rpc::Frame::Notification { .. } | rpc::Frame::Other => {}
            rpc::Frame::Response { id: None, .. } => {}
            rpc::Frame::Response {
                id: Some(id),
                outcome,
            } => {
                scoped = self
                    .turn
                    .as_ref()
                    .is_some_and(|turn| turn.requests.contains(&id));
                self.respond(id, outcome, &mut step);
            }
        }
        if scoped && let Some(run) = run {
            step.events.push(Event::Execution {
                run,
                event: crate::agent_execution::ProviderEvent::Scope,
            });
        }
        Ok(step)
    }
}

/// The profile the adapter's own tests and the fake agent run under. It
/// passes the command line through, taking the session settings from
/// `--model`, `--reasoning` and `--mode`.
#[cfg(test)]
pub(crate) static TEST_PROFILE: Profile = Profile {
    label: "Fake ACP agent",
    launch: test_launch,
    max_image_bytes: 1024 * 1024,
    modes: &[
        ("default", "default"),
        ("accept-edits", "acceptEdits"),
        ("auto", "auto"),
        ("plan", "plan"),
    ],
    offers_model: |model| !model.ends_with("-contributor"),
    sign_in_required: auth_required,
    steers: true,
    cancel_stops_background: true,
    backgrounds_commands: true,
};

#[cfg(test)]
fn test_launch(launch: &Launch) -> Result<Startup, String> {
    let mut settings = Settings::default();
    let mut arguments = Vec::new();
    let mut given = launch.recipe.arguments.iter();
    while let Some(argument) = given.next() {
        let slot = match argument.as_str() {
            "--model" => &mut settings.model,
            "--reasoning" => &mut settings.reasoning,
            "--mode" => &mut settings.mode,
            _ => {
                arguments.push(argument.clone());
                continue;
            }
        };
        *slot = Some(
            given
                .next()
                .ok_or_else(|| format!("Missing value for {argument}"))?
                .clone(),
        );
    }
    Ok(Startup {
        program: launch.recipe.program.clone(),
        arguments,
        settings,
    })
}
