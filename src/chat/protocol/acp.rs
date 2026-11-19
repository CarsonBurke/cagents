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
use super::{Adapter, ProcessSpec, Step};
use crate::chat::{
    self, Capabilities, Choice, Command, Event, Item, ItemContent, Launch, PlanState, PlanStep,
    PromptMode, Request, RequestKind, Role, SkillReach, SlashCommand,
};
use crate::notifications::Kind;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};

mod config;
mod history;
mod prompt;
mod rpc;
#[cfg(test)]
mod tests;
mod tools;

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
    /// Whether an error reply means the user has to sign in again. ACP
    /// reserves `-32000` for this; [`auth_required`] checks only that.
    pub sign_in_required: fn(&Value) -> bool,
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

/// How a sign-in failure begins, which [`chat::requires_sign_in`]'s phrases
/// recognise once an ACP provider is admitted there.
pub const SIGN_IN_PREFIX: &str = "Authentication required";

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
    Setting(Setting),
    Prompt,
}

/// The prompt in flight, from its request until its reply.
struct Turn {
    request: u64,
    key: String,
    /// `session/cancel` has been sent; the reply decides how the turn ended.
    cancelling: bool,
    run: Option<crate::model::RunId>,
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
}

struct Acp {
    profile: &'static Profile,
    cwd: String,
    resume: Option<String>,
    stage: Stage,
    next_request: u64,
    pending: HashMap<u64, Pending>,
    session: Option<String>,
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

    /// The message a reader is shown for an error reply: its own words, or
    /// for a sign-in failure, what to do about it.
    fn failure(&self, error: &Value) -> String {
        let message = rpc::error_message(error);
        if (self.profile.sign_in_required)(error) {
            format!(
                "{SIGN_IN_PREFIX}: sign in to {}, then try again. ({message})",
                self.profile.label
            )
        } else {
            message
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
            // cagents holds a message sent during a turn until the turn ends.
            steer: false,
            queue: false,
            attachments: true,
            compact: self.compacts(),
            model_selection: self.config.model_selection(),
            reasoning_selection: self.config.reasoning_selection(),
            permission_modes: self.config.mode_ids(),
            skill_reach: SkillReach::Leading,
            ..Capabilities::default()
        })
    }

    fn compacts(&self) -> bool {
        self.commands
            .iter()
            .any(|command| command.name == "compact")
    }

    fn configuration_changed(&self, step: &mut Step) {
        step.events.extend(self.config.events());
        if self.stage == Stage::Ready {
            step.events.push(self.capabilities());
        }
    }

    /// Sends the next queued setting, if none is in flight. Returns whether
    /// one is now outstanding.
    fn send_setting(&mut self, step: &mut Step) -> bool {
        while !self.setting_in_flight {
            let Some(setting) = self.settings.pop_front() else {
                return false;
            };
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
        let params = json!({"sessionId": self.session_id(), "prompt": blocks});
        let (id, frame) = self.request("session/prompt", params, Pending::Prompt);
        step.writes.push(frame);
        self.segment = None;
        self.turn = Some(Turn {
            request: id,
            key: format!("acp-prompt:{}:{id}", self.session_id()),
            cancelling: false,
            run: None,
        });
        step.events.extend(items.into_iter().map(Event::Item));
        step.events.push(Event::TurnStarted);
        step.events.push(Event::Status {
            busy: true,
            detail: "Working".into(),
        });
        Ok(step)
    }

    fn end_turn(&mut self, outcome: Result<&Value, &Value>, step: &mut Step) {
        let Some(turn) = self.turn.take() else {
            return;
        };
        self.segment = None;
        let (kind, notice) = match outcome {
            Ok(result) => {
                // The unstable `PromptResponse.usage` is read as this turn's
                // own tokens, so turns add up; an agent that reports a
                // running total instead would be counted again each turn.
                if let Some(usage) = result.get("usage").filter(|usage| usage.is_object()) {
                    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
                    self.usage.input_tokens += count("inputTokens");
                    self.usage.output_tokens += count("outputTokens");
                    step.events.push(Event::Usage(self.usage.clone()));
                }
                match result["stopReason"].as_str() {
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
                }
            }
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
        step.events.push(Event::Status {
            busy: false,
            detail: "Ready".into(),
        });
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
            Pending::Setting(setting) => {
                self.setting_in_flight = false;
                match outcome {
                    Ok(result) => {
                        if let Some(options) = result["configOptions"].as_array() {
                            self.config.learn_options(options);
                        }
                        match &setting {
                            Setting::Model(model) => self.config.model_changed(model),
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
            Pending::Prompt => self.end_turn(outcome, step),
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
            step.events.push(Event::History {
                items: replay.finish(),
                has_earlier: false,
            });
        }
        self.segment = None;
        step.events.push(Event::Session {
            id: self.session_id().to_owned(),
            title: None,
        });
        self.config.learn(result);
        self.configuration_changed(step);
        self.stage = Stage::Configuring;
        if !self.send_setting(step) {
            self.become_ready(step);
        }
    }

    fn permission(&mut self, rpc: &Value, params: &Value, step: &mut Step) {
        let call = &params["toolCall"];
        let call_id = call["toolCallId"].as_str().unwrap_or_default().to_owned();
        if let Some((id, content)) = self.calls.merge(call) {
            self.segment = None;
            step.events.push(Event::Item(tool_item(id, content)));
        }
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
        match update["sessionUpdate"].as_str().unwrap_or_default() {
            kind @ ("user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk") => {
                let role = if kind == "user_message_chunk" {
                    Role::User
                } else {
                    Role::Assistant
                };
                // The reader's own message is shown when it is sent; an agent
                // that echoes it during the turn would show it twice.
                if role == Role::User && self.replay.is_none() && self.turn.is_some() {
                    return;
                }
                self.chunk(role, kind == "agent_thought_chunk", update, &mut events);
            }
            "tool_call" | "tool_call_update" => {
                if let Some((id, content)) = self.calls.merge(update) {
                    self.segment = None;
                    events.push(Event::Item(tool_item(id, content)));
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
                    events.push(Event::PermissionMode(Some(mode.to_owned())));
                }
            }
            "config_option_update" => {
                if let Some(options) = update["configOptions"].as_array() {
                    self.config.learn_options(options);
                    let mut changed = Step::default();
                    self.configuration_changed(&mut changed);
                    events.extend(changed.events);
                }
            }
            "session_info_update" => {
                if let Some(title) = update["title"]
                    .as_str()
                    .map(str::trim)
                    .filter(|title| !title.is_empty())
                {
                    events.push(Event::Session {
                        id: self.session_id().to_owned(),
                        title: Some(title.to_owned()),
                    });
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
            events: vec![Event::Status {
                busy: false,
                detail: "Starting".into(),
            }],
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
                if mode != PromptMode::Send {
                    return Err(format!(
                        "{label} cannot take a message while it is working; send it once this turn ends."
                    ));
                }
                self.accepts_turn()?;
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
                let step = self.start_turn(prompt.blocks, prompt.items)?;
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
                let permission = self.permissions.remove(&request_id).unwrap();
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
                if !self.config.offers_mode(&mode) {
                    return Err(format!("{label} does not offer the mode {mode:?}."));
                }
                self.settings.push_back(Setting::Mode(mode));
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
            Command::RemoveQueued { .. } | Command::StartQueued { .. } => {
                Err(format!("{label} keeps no queue of its own."))
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
                scoped = self.turn.as_ref().is_some_and(|turn| turn.request == id);
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
    sign_in_required: auth_required,
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
