use super::{Adapter, ProcessSpec, Step};
use crate::chat::{
    self, Answer, ChangeKind, Choice, Command, CommandAction, Event, Field, FileChange, Item,
    ItemContent, Launch, Model, Phase, PromptMode, Request, Role, ToolStatus,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::Read,
};

const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
const MAX_IMAGE: u64 =
    chat::AttachmentRules::for_provider(crate::agent::Provider::Omp).max_image_bytes;
/// The same bound the Codex adapter puts on the items it keeps live, so the two
/// adapters retain a comparable amount of a session's tool history.
const MAX_CALLS: usize = 128;

pub fn create(launch: &Launch) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    let mut spec = ProcessSpec::from(&launch.recipe);
    let mut args = launch.recipe.arguments.iter();
    let mut kept = Vec::new();
    while let Some(arg) = args.next() {
        let (name, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(a, b)| (a, Some(b)));
        if matches!(name, "--resume" | "-r" | "--mode") {
            if inline.is_none() {
                args.next()
                    .ok_or_else(|| format!("Missing value for {name}"))?;
            }
            continue;
        }
        let value_flag = matches!(
            name,
            "--model"
                | "--provider"
                | "--profile"
                | "--session-dir"
                | "--thinking"
                | "--config"
                | "--add-dir"
                | "--approval-mode"
                | "--smol"
                | "--slow"
                | "--plan"
                | "--models"
                | "--service-tier"
                | "--tools"
                | "--skills"
                | "--extension"
                | "-e"
                | "--hook"
                | "--plugin-dir"
                | "--append-system-prompt"
                | "--system-prompt"
        );
        let switch = matches!(
            name,
            "--no-lsp"
                | "--no-tools"
                | "--no-extensions"
                | "--no-skills"
                | "--no-rules"
                | "--no-pty"
                | "--no-title"
                | "--auto-approve"
                | "--hide-thinking"
                | "--advisor"
        );
        if !value_flag && !switch {
            return Err(format!(
                "Cannot safely transfer OMP launch option {name:?}; use an explicit resume launch without an initial prompt."
            ));
        }
        kept.push(arg.clone());
        if value_flag && inline.is_none() {
            kept.push(
                args.next()
                    .ok_or_else(|| format!("Missing value for {name}"))?
                    .clone(),
            );
        }
    }
    kept.extend(["--mode".into(), "rpc-ui".into()]);
    if let Some(id) = &launch.session_id {
        if id.len() != 36 || uuid::Uuid::parse_str(id).is_err() {
            return Err("OMP requires an exact session UUID for native resume.".into());
        }
        kept.extend(["--resume".into(), id.clone()]);
    }
    spec.arguments = kept;
    Ok((
        spec,
        Box::new(Omp {
            expected_session: launch.session_id.clone(),
            ..Omp::default()
        }),
    ))
}

#[derive(Default)]
struct Omp {
    sequence: u64,
    ready: bool,
    busy: bool,
    expected_session: Option<String>,
    session_id: Option<String>,
    state_request: Option<String>,
    history_request: Option<String>,
    requests: HashMap<String, String>,
    chunks: HashMap<String, Chunk>,
    calls: HashMap<String, Call>,
    call_order: VecDeque<String>,
    usage: chat::Usage,
}
struct Chunk {
    next: usize,
    count: usize,
    bytes: usize,
    data: Vec<u8>,
}
/// What OMP classified a tool call as, together with the arguments it ran with.
///
/// OMP splits a call across frames: the arguments come with its start and the
/// result with its end, and a history separates the same two halves into an
/// assistant `toolCall` block and the `toolResult` message answering it. A
/// command's text and directory have to be carried across that gap or the
/// finished step would render without the command it ran. A completed call is
/// kept rather than dropped because a live session reports the same result
/// twice, once as `tool_execution_end` and once as the `toolResult` message.
struct Call {
    command: String,
    working_directory: String,
    action: CommandAction,
}
impl Call {
    /// The step this call shows, at whatever stage it has reached.
    fn item(&self, status: ToolStatus, output: String, details: &Value) -> ItemContent {
        ItemContent::Command {
            // A shell step has no target beyond the command line the view
            // already falls back to. Every other action was given the path or
            // pattern it acted on, which is the target worth naming.
            label: match self.action {
                CommandAction::Run => String::new(),
                _ => self.command.clone(),
            },
            command: self.command.clone(),
            working_directory: self.working_directory.clone(),
            action: self.action,
            status,
            // Reported, never classified: the status comes from OMP's own
            // `isError` flag, so a command that exits non-zero to answer the
            // question it was asked - a grep with no matches, a `git diff
            // --quiet` - shows its code without being called a failure.
            exit_code: details["exitCode"]
                .as_i64()
                .and_then(|code| i32::try_from(code).ok()),
            duration_ms: duration(details),
            output,
        }
    }
}
impl Omp {
    fn request(&mut self, kind: &str) -> Value {
        self.sequence = self.sequence.wrapping_add(1);
        let id = format!("cagents-{}", self.sequence);
        if kind == "get_state" {
            self.state_request = Some(id.clone());
        }
        json!({"id":id,"type":kind})
    }
    fn refresh(&mut self) -> Step {
        let mut step = Step::default();
        for kind in [
            "get_state",
            "get_available_models",
            "get_available_commands",
            "get_session_stats",
        ] {
            step.writes.push(self.request(kind));
        }
        step
    }
    fn history_page(&mut self, cursor: Option<&str>) -> Value {
        let mut request = self.request("get_messages_page");
        request["limit"] = 100.into();
        if let Some(cursor) = cursor {
            request["cursor"] = cursor.into();
        }
        self.history_request = request["id"].as_str().map(str::to_owned);
        request
    }
    fn message_items(&mut self, message: &Value) -> Vec<Item> {
        let role_name = text(message, "role");
        let role = match role_name {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => Role::System,
        };
        if role_name == "toolResult" {
            let id = text(message, "toolCallId").to_owned();
            let status = if message["isError"].as_bool() == Some(true) {
                ToolStatus::Failed
            } else {
                ToolStatus::Succeeded
            };
            let content = self.result(
                &id,
                text(message, "toolName"),
                content_text(&message["content"]),
                &message["details"],
                status,
            );
            return vec![Item {
                id: format!("tool:{id}"),
                role: Role::Assistant,
                content,
                at: None,
            }];
        }
        let key = message_key(message);
        if let Some(content) = message["content"].as_str() {
            return vec![Item {
                id: format!("{key}:0"),
                role,
                content: ItemContent::Text {
                    text: content.into(),
                    thinking: false,
                    phase: Phase::Final,
                },
                at: None,
            }];
        }
        message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .filter_map(|(index, block)| {
                let (id, content) = match text(block, "type") {
                    "text" => (
                        format!("{key}:{index}"),
                        ItemContent::Text {
                            text: text(block, "text").into(),
                            thinking: false,
                            phase: phase(block),
                        },
                    ),
                    "thinking" => (
                        format!("{key}:{index}"),
                        ItemContent::Text {
                            text: text(block, "thinking").into(),
                            thinking: true,
                            phase: Phase::Final,
                        },
                    ),
                    "toolCall" => {
                        let id = text(block, "id");
                        let content = self.running(
                            id,
                            text(block, "name"),
                            &block["arguments"],
                            String::new(),
                        );
                        (format!("tool:{id}"), content)
                    }
                    "image" => (
                        format!("{key}:{index}"),
                        ItemContent::Image {
                            path: format!(
                                "data:{};base64,{}",
                                text(block, "mimeType"),
                                text(block, "data")
                            ),
                            description: "Image attachment".into(),
                        },
                    ),
                    _ => return None,
                };
                Some(Item {
                    id,
                    role,
                    content,
                    at: None,
                })
            })
            .collect()
    }
    /// The item a tool call produces while it is still running.
    ///
    /// The first classification recorded for a call is the one kept. An update
    /// frame repeats the call without repeating its arguments, so classifying
    /// it again would replace the command the call started with by an empty
    /// one, which is exactly what the recorded call exists to prevent.
    fn running(&mut self, id: &str, name: &str, args: &Value, output: String) -> ItemContent {
        if let Some(call) = self.calls.get(id) {
            // A running command has neither an exit status nor a duration.
            return call.item(ToolStatus::Running, output, &Value::Null);
        }
        match call(name, args) {
            Some(call) => {
                let content = call.item(ToolStatus::Running, output, &Value::Null);
                // A classification that arrived without the arguments it was
                // made from is not worth carrying to the call's result, and
                // leaving it unrecorded lets a later frame that does carry
                // them record the call instead.
                if !call.command.is_empty() {
                    self.record(id, call);
                }
                content
            }
            None => ItemContent::Tool {
                name: name.into(),
                input: arguments(args),
                output,
                status: ToolStatus::Running,
            },
        }
    }
    /// Retain a classified call, evicting the oldest once the bound is reached.
    ///
    /// A finished call cannot be dropped when it finishes, because its result
    /// arrives twice, so a day-long session would otherwise keep every command
    /// it ever ran inside a daemon the user cannot restart.
    fn record(&mut self, id: &str, call: Call) {
        if !self.calls.contains_key(id) {
            self.call_order.push_back(id.to_owned());
        }
        self.calls.insert(id.to_owned(), call);
        while self.call_order.len() > MAX_CALLS {
            if let Some(evicted) = self.call_order.pop_front() {
                self.calls.remove(&evicted);
            }
        }
    }
    /// The item a finished tool call produces.
    ///
    /// The arguments a command needs come from the call this adapter recorded
    /// when the tool started, because a result frame carries only the result.
    fn result(
        &self,
        id: &str,
        name: &str,
        output: String,
        details: &Value,
        status: ToolStatus,
    ) -> ItemContent {
        if let Some(call) = self.calls.get(id) {
            return call.item(status, output, details);
        }
        let changes = changes(details);
        if changes.is_empty() {
            return ItemContent::Tool {
                name: name.into(),
                input: String::new(),
                output,
                status,
            };
        }
        ItemContent::Changes { changes, status }
    }
    fn extension(&mut self, frame: &Value) -> Result<Step, String> {
        let id = text(frame, "id").to_owned();
        let method = text(frame, "method");
        let mut step = Step::default();
        match method {
            "select" | "confirm" | "input" | "editor" => {
                if id.is_empty() {
                    return Err("OMP interaction is missing its request ID.".into());
                }
                self.requests.insert(id.clone(), method.into());
                let choices = frame["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .filter_map(|(index, option)| {
                        Some(Choice {
                            id: option.as_str()?.into(),
                            label: option.as_str()?.into(),
                            description: frame["optionDetails"][index]["description"]
                                .as_str()
                                .unwrap_or_default()
                                .into(),
                        })
                    })
                    .collect();
                let fields = if method == "confirm" {
                    Vec::new()
                } else {
                    vec![Field {
                        id: "value".into(),
                        label: text(frame, "title").into(),
                        description: text(frame, "placeholder").into(),
                        choices,
                        multiple: false,
                        secret: false,
                        initial: text(frame, "prefill").into(),
                    }]
                };
                step.events.push(Event::Request(Request {
                    id,
                    title: text(frame, "title").into(),
                    description: text(frame, "message").into(),
                    fields,
                    actions: vec![
                        Choice {
                            id: "submit".into(),
                            label: if method == "confirm" {
                                "Confirm"
                            } else {
                                "Submit"
                            }
                            .into(),
                            description: String::new(),
                        },
                        Choice {
                            id: "cancel".into(),
                            label: "Cancel".into(),
                            description: String::new(),
                        },
                    ],
                }));
            }
            "cancel" => {
                let target = text(frame, "targetId");
                self.requests.remove(target);
                step.events
                    .push(Event::RequestResolved { id: target.into() });
            }
            "notify" => {
                self.sequence += 1;
                step.events.push(Event::Item(Item {
                    id: format!("notice:{}", self.sequence),
                    role: Role::System,
                    content: ItemContent::Text {
                        text: text(frame, "message").into(),
                        thinking: false,
                        phase: Phase::Final,
                    },
                    at: None,
                }));
            }
            "setStatus" => step.events.push(Event::Status {
                busy: self.busy,
                detail: text(frame, "statusText").into(),
            }),
            "setWidget" => {
                self.sequence += 1;
                step.events.push(Event::Item(Item {
                    id: format!("widget:{}", text(frame, "widgetKey")),
                    role: Role::System,
                    content: ItemContent::Text {
                        text: frame["widgetLines"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n"),
                        thinking: false,
                        phase: Phase::Final,
                    },
                    at: None,
                }));
            }
            "setTitle" => {}
            "set_editor_text" => step.events.push(Event::Composer {
                text: text(frame, "text").into(),
            }),
            "open_url" => step.events.push(Event::OpenUrl {
                url: frame["launchUrl"]
                    .as_str()
                    .unwrap_or_else(|| text(frame, "url"))
                    .into(),
                instructions: text(frame, "instructions").into(),
            }),
            _ => {
                if !id.is_empty() {
                    step.writes
                        .push(json!({"type":"extension_ui_response","id":id,"cancelled":true}));
                }
                step.events.push(Event::Error {
                    message: format!("OMP requested an unsupported interactive method: {method}"),
                    fatal: false,
                });
            }
        }
        Ok(step)
    }
}
impl Adapter for Omp {
    fn initialize(&mut self) -> Result<Step, String> {
        Ok(Step::default())
    }
    fn command(&mut self, command: Command) -> Result<Step, String> {
        if !self.ready {
            return Err("OMP is still initializing its structured connection.".into());
        }
        let mut step = Step::default();
        match command {
            Command::Prompt{text:message,attachments,mode} => {
                let kind=match mode {PromptMode::Send=>"prompt",PromptMode::Steer=>"steer",PromptMode::Queue=>"follow_up"};
                let mut request=self.request(kind);
                let mut message=message;
                let mut images=Vec::new();
                for attachment in attachments {
                    let path=attachment.path.canonicalize().map_err(|e|format!("Cannot attach {}: {e}",attachment.path.display()))?;
                    let ext=path.extension().and_then(|s|s.to_str()).unwrap_or_default().to_ascii_lowercase();
                    let mime=match ext.as_str(){"png"=>Some("image/png"),"jpg"|"jpeg"=>Some("image/jpeg"),"gif"=>Some("image/gif"),"webp"=>Some("image/webp"),_=>None};
                    if let Some(mime)=mime {
                        let mut bytes=Vec::new(); File::open(&path).map_err(|e|e.to_string())?.take(MAX_IMAGE+1).read_to_end(&mut bytes).map_err(|e|e.to_string())?;
                        if bytes.len() as u64>MAX_IMAGE {return Err("Image exceeds the 16 MiB attachment limit.".into());}
                        images.push(json!({"type":"image","mimeType":mime,"data":STANDARD.encode(bytes)}));
                    } else {
                        message.push_str("\nAttached file (read this exact path): "); message.push_str(&serde_json::to_string(&path.to_string_lossy()).map_err(|e|e.to_string())?);
                    }
                }
                request["message"]=message.into(); if !images.is_empty(){request["images"]=images.into();}
                step.writes.push(request);
            }
            Command::Interrupt => step.writes.push(self.request("abort")),
            Command::Answer{request_id,action,answers} => {
                let method=self.requests.get(&request_id).ok_or("This OMP request is no longer pending.")?;
                let response=if action=="cancel" {json!({"type":"extension_ui_response","id":request_id,"cancelled":true})}
                else if action!="submit" {return Err("Unknown OMP interaction response.".into());}
                else if method=="confirm" {json!({"type":"extension_ui_response","id":request_id,"confirmed":true})}
                else {json!({"type":"extension_ui_response","id":request_id,"value":answer_value(&answers)})};
                self.requests.remove(&request_id); step.writes.push(response); step.events.push(Event::RequestResolved{id:request_id});
            }
            Command::SetModel{model,reasoning} => {
                let (provider,id)=model.split_once('/').ok_or("Select a provider-qualified OMP model.")?;
                let mut request=self.request("set_model"); request["provider"]=provider.into(); request["modelId"]=id.into(); step.writes.push(request);
                if let Some(level)=reasoning {let mut request=self.request("set_thinking_level"); request["level"]=level.into();step.writes.push(request);}
            }
            Command::SetPermissionMode{..} => return Err("OMP approval policy is configured at launch; use its settings command to change policy.".into()),
            Command::Compact => step.writes.push(self.request("compact")),
            Command::Refresh => return Ok(self.refresh()),
            Command::LoadEarlier => return Err("OMP history is loaded completely during connection.".into()),
            Command::Slash{command} => {let mut request=self.request("prompt");request["message"]=command.into();step.writes.push(request);}
        }
        Ok(step)
    }
    fn receive(&mut self, frame: Value) -> Result<Step, String> {
        let mut step = Step::default();
        match text(&frame, "type") {
            "ready" => {
                let mut request = self.request("negotiate_protocol");
                request["protocolVersion"] = 2.into();
                step.writes.push(request);
            }
            "rpc_chunk" => {
                let id = text(&frame, "chunkId").to_owned();
                let index = frame["index"].as_u64().ok_or("Invalid OMP chunk index")? as usize;
                let count = frame["count"].as_u64().ok_or("Invalid OMP chunk count")? as usize;
                let bytes = frame["byteLength"]
                    .as_u64()
                    .ok_or("Invalid OMP chunk size")? as usize;
                if count == 0 || count > 1024 || bytes > MAX_PAYLOAD || self.chunks.len() > 4 {
                    return Err("OMP chunked frame exceeds protocol limits.".into());
                }
                let chunk = self.chunks.entry(id.clone()).or_insert_with(|| Chunk {
                    next: 0,
                    count,
                    bytes,
                    data: Vec::with_capacity(bytes),
                });
                if chunk.next != index || chunk.count != count || chunk.bytes != bytes {
                    return Err("Out-of-order OMP chunked frame.".into());
                }
                let decoded = STANDARD
                    .decode(text(&frame, "data"))
                    .map_err(|e| e.to_string())?;
                if chunk.data.len() + decoded.len() > bytes {
                    return Err("OMP chunk exceeds declared size.".into());
                }
                chunk.data.extend_from_slice(&decoded);
                chunk.next += 1;
                if chunk.next == count {
                    let chunk = self.chunks.remove(&id).unwrap();
                    if chunk.data.len() != bytes {
                        return Err("Incomplete OMP chunked frame.".into());
                    }
                    return self
                        .receive(serde_json::from_slice(&chunk.data).map_err(|e| e.to_string())?);
                }
            }
            "response" => {
                // Replies may arrive after a local extension has switched sessions.
                // Only the current state request and current page may mutate the view.
                let pending = match text(&frame, "command") {
                    "get_state" => Some(&mut self.state_request),
                    "get_messages_page" => Some(&mut self.history_request),
                    _ => None,
                };
                if let Some(pending) = pending {
                    if pending.as_deref() != frame["id"].as_str() || pending.is_none() {
                        return Ok(step);
                    }
                    *pending = None;
                }
                if frame["success"].as_bool() != Some(true) {
                    // The conversation takes commands only once these have
                    // answered, so one that fails leaves it never ready; it
                    // ends with the reason rather than waiting on nothing.
                    let fatal = !self.ready
                        && matches!(
                            text(&frame, "command"),
                            "negotiate_protocol" | "get_state" | "get_messages_page"
                        );
                    step.events.push(Event::Error {
                        message: text(&frame, "error").into(),
                        fatal,
                    });
                    return Ok(step);
                }
                let data = &frame["data"];
                match text(&frame, "command") {
                    "negotiate_protocol" => step = self.refresh(),
                    "get_state" => {
                        let id = text(data, "sessionId");
                        if id.is_empty() {
                            return Err("OMP did not identify its conversation.".into());
                        }
                        if self
                            .expected_session
                            .as_deref()
                            .is_some_and(|expected| expected != id)
                        {
                            return Err(
                                "OMP resumed a different conversation; refusing native control."
                                    .into(),
                            );
                        }
                        self.expected_session = None;
                        if self
                            .session_id
                            .as_deref()
                            .is_some_and(|previous| previous != id)
                        {
                            self.ready = false;
                            self.history_request = None;
                            self.requests.clear();
                            self.calls.clear();
                            self.call_order.clear();
                            self.usage = chat::Usage::default();
                            step.events.push(Event::Reset {
                                reason: "OMP changed the current conversation.".into(),
                            });
                            step.events.push(Event::Ready(false));
                        }
                        self.session_id = Some(id.into());
                        self.busy = data["isStreaming"].as_bool().unwrap_or(false)
                            || data["isCompacting"].as_bool().unwrap_or(false);
                        if !self.ready && !self.busy && self.history_request.is_none() {
                            step.writes.push(self.history_page(None));
                        }
                        step.events.push(Event::Session {
                            id: id.into(),
                            title: session_title(&data["sessionName"]),
                        });
                        step.events.push(Event::Status {
                            busy: self.busy,
                            detail: if !self.ready {
                                "Loading conversation"
                            } else if self.busy {
                                "Working"
                            } else {
                                "Ready"
                            }
                            .into(),
                        });
                        step.events.push(Event::Capabilities(chat::Capabilities {
                            steer: true,
                            queue: true,
                            attachments: true,
                            compact: true,
                            model_selection: true,
                            reasoning_selection: true,
                            permission_modes: Vec::new(),
                            skill_reach: chat::SkillReach::First,
                        }));
                        step.events.push(Event::Models {
                            models: Vec::new(),
                            selected: model_id(&data["model"]),
                            reasoning: data["thinkingLevel"].as_str().map(str::to_owned),
                        });
                        let usage = &data["contextUsage"];
                        self.usage.context_tokens = usage["tokens"].as_u64().unwrap_or(0);
                        self.usage.context_limit = usage["contextWindow"].as_u64().unwrap_or(0);
                        step.events.push(Event::Usage(self.usage.clone()));
                    }
                    "get_available_models" => {
                        let models = data["models"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|model| {
                                Some(Model {
                                    id: model_id(model)?,
                                    label: model["name"]
                                        .as_str()
                                        .unwrap_or_else(|| text(model, "id"))
                                        .into(),
                                    reasoning: [
                                        "off", "minimal", "low", "medium", "high", "xhigh", "max",
                                        "auto",
                                    ]
                                    .into_iter()
                                    .map(str::to_owned)
                                    .collect(),
                                })
                            })
                            .collect();
                        step.events.push(Event::Models {
                            models,
                            selected: None,
                            reasoning: None,
                        });
                    }
                    "get_available_commands" => step.events.extend(commands(data)),
                    "get_messages_page" => {
                        for message in data["messages"].as_array().into_iter().flatten() {
                            step.events
                                .extend(self.message_items(message).into_iter().map(Event::Item));
                        }
                        if let Some(cursor) = data["nextCursor"].as_str() {
                            step.writes.push(self.history_page(Some(cursor)));
                        } else {
                            self.ready = true;
                            step.events.push(Event::Ready(true));
                            step.events.push(Event::Status {
                                busy: self.busy,
                                detail: if self.busy { "Working" } else { "Ready" }.into(),
                            });
                        }
                    }
                    "get_session_stats" => {
                        let tokens = &data["tokens"];
                        self.usage.input_tokens = tokens["input"].as_u64().unwrap_or(0);
                        self.usage.output_tokens = tokens["output"].as_u64().unwrap_or(0);
                        self.usage.cost_microusd = data["cost"]
                            .as_f64()
                            .filter(|n| n.is_finite() && *n >= 0.0)
                            .map(|n| (n * 1_000_000.0) as u64);
                        step.events.push(Event::Usage(self.usage.clone()));
                    }
                    "set_model" | "set_thinking_level" | "compact" => step = self.refresh(),
                    "prompt" if data["agentInvoked"].as_bool() == Some(false) => {
                        step = self.refresh()
                    }
                    _ => {}
                }
            }
            "message_start" | "message_end" => {
                step.events.extend(
                    self.message_items(&frame["message"])
                        .into_iter()
                        .map(Event::Item),
                );
            }
            "message_update" => {
                let event = &frame["assistantMessageEvent"];
                let kind = text(event, "type");
                if matches!(kind, "text_delta" | "thinking_delta") {
                    step.events.push(Event::TextDelta {
                        id: format!(
                            "{}:{}",
                            message_key(&frame["message"]),
                            event["contentIndex"].as_u64().unwrap_or(0)
                        ),
                        role: Role::Assistant,
                        thinking: kind == "thinking_delta",
                        delta: text(event, "delta").into(),
                    });
                }
            }
            "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
                let id = text(&frame, "toolCallId").to_owned();
                let name = text(&frame, "toolName");
                let content = if text(&frame, "type") == "tool_execution_end" {
                    let status = if frame["isError"].as_bool() == Some(true) {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Succeeded
                    };
                    self.result(
                        &id,
                        name,
                        content_text(&frame["result"]["content"]),
                        &frame["result"]["details"],
                        status,
                    )
                } else {
                    self.running(
                        &id,
                        name,
                        &frame["args"],
                        content_text(&frame["partialResult"]["content"]),
                    )
                };
                step.events.push(Event::Item(Item {
                    id: format!("tool:{id}"),
                    role: Role::Assistant,
                    content,
                    at: None,
                }));
            }
            "command_output" => {
                self.sequence += 1;
                step.events.push(Event::Item(Item {
                    id: format!("command:{}", self.sequence),
                    role: Role::System,
                    content: ItemContent::Text {
                        text: text(&frame, "text").into(),
                        thinking: false,
                        phase: Phase::Final,
                    },
                    at: None,
                }));
            }
            "tool_stream_update" => {
                step.events.push(Event::Item(Item {
                    id: format!("tool:{}", text(&frame, "toolCallId")),
                    role: Role::Assistant,
                    content: ItemContent::Tool {
                        name: text(&frame, "toolName").into(),
                        input: String::new(),
                        output: frame["update"].to_string(),
                        status: ToolStatus::Running,
                    },
                    at: None,
                }));
            }
            "subagent_lifecycle" | "subagent_progress" => {
                let payload = &frame["payload"];
                let status = match text(payload, "status") {
                    "completed" => ToolStatus::Succeeded,
                    "failed" | "aborted" => ToolStatus::Failed,
                    _ => ToolStatus::Running,
                };
                step.events.push(Event::Item(Item {
                    id: format!("subagent:{}", text(payload, "id")),
                    role: Role::Assistant,
                    content: ItemContent::Tool {
                        name: format!("Agent {}", text(payload, "agent")),
                        input: text(payload, "task").into(),
                        output: payload.to_string(),
                        status,
                    },
                    at: None,
                }));
            }
            "agent_start" | "auto_compaction_start" => {
                self.busy = true;
                step.events.push(Event::Status {
                    busy: true,
                    detail: if text(&frame, "type") == "agent_start" {
                        "Working"
                    } else {
                        "Compacting"
                    }
                    .into(),
                });
            }
            "agent_end" | "auto_compaction_end" => {
                self.busy = false;
                step = self.refresh();
                step.events.push(Event::Status {
                    busy: false,
                    detail: "Ready".into(),
                });
            }
            "extension_ui_request" => return self.extension(&frame),
            "available_commands_update" => step.events.extend(commands(&frame)),
            "prompt_result" if frame["agentInvoked"].as_bool() == Some(false) => {
                step = self.refresh()
            }
            _ => {}
        }
        Ok(step)
    }
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}
/// The conversation's own name, or `None` when OMP labelled the session with
/// something else.
///
/// The surface draws this as the chat's heading, and `sessionName` carries
/// whatever last labelled the session. A harness that routes a stream of
/// events through a session labels it with the kind of the event - `irc`,
/// `system_notice` - and the reader then reads an internal event kind as the
/// name of their own conversation. A heading is the conversation's title or
/// nothing, so the label is refused here, where it enters the model, rather
/// than being dressed up in the view.
///
/// The refusal is by shape rather than by a list of known kinds, because a
/// harness is free to invent one: a title is written for a person and reads
/// as prose, while an event kind, a channel and a notice category are single
/// lower-case slugs.
fn session_title(value: &Value) -> Option<String> {
    let name = value.as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let slug = name.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '_' | '-' | '.' | '/' | ':' | '#')
    });
    (!slug).then(|| name.to_owned())
}
fn model_id(value: &Value) -> Option<String> {
    Some(format!(
        "{}/{}",
        value["provider"].as_str()?,
        value["id"].as_str()?
    ))
}
fn message_key(value: &Value) -> String {
    format!(
        "{}:{}",
        text(value, "role"),
        value
            .get("timestamp")
            .map(Value::to_string)
            .unwrap_or_else(|| "missing".into())
    )
}
fn content_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.into();
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
/// What OMP says a tool call was for.
///
/// A tool's registered name is the provider's own classification, so `bash` is
/// a shell command and `read` is a file read without anything here having to
/// interpret arguments. A name outside this set is left to the `Tool` fallback
/// rather than guessed at.
fn call(name: &str, args: &Value) -> Option<Call> {
    let target = |key: &str, action: CommandAction| Call {
        command: text(args, key).into(),
        working_directory: String::new(),
        action,
    };
    match name {
        "bash" => Some(Call {
            command: text(args, "command").into(),
            working_directory: text(args, "cwd").into(),
            action: CommandAction::Run,
        }),
        "read" => Some(target("path", CommandAction::Read)),
        "grep" | "search" => Some(target("pattern", CommandAction::Search)),
        "glob" | "find" => Some(target("path", CommandAction::List)),
        _ => None,
    }
}
/// A tool call's arguments as the `Tool` fallback shows them.
fn arguments(args: &Value) -> String {
    if args.is_null() {
        String::new()
    } else {
        args.to_string()
    }
}
/// The files an edit reports changing, one entry per path.
///
/// OMP's edit tool returns the unified diff it applied, split per path when it
/// touched several, which is what lets the view render the change itself rather
/// than a sentence about it. A result carrying no diff is not an edit and stays
/// with the `Tool` fallback.
fn changes(details: &Value) -> Vec<FileChange> {
    match details["perFileResults"].as_array() {
        Some(results) => results.iter().filter_map(change).collect(),
        None => change(details).into_iter().collect(),
    }
}
fn change(result: &Value) -> Option<FileChange> {
    if result["isError"].as_bool() == Some(true) {
        return None;
    }
    let diff = result["diff"].as_str().filter(|diff| !diff.is_empty())?;
    let path = result["path"].as_str().filter(|path| !path.is_empty())?;
    Some(FileChange {
        path: path.into(),
        // A moved file reports the path it moved to, so the path it came from is
        // the one the result keeps aside.
        kind: match (result["sourcePath"].as_str(), text(result, "op")) {
            (Some(from), _) => ChangeKind::Renamed { from: from.into() },
            (None, "create") => ChangeKind::Added,
            (None, "delete") => ChangeKind::Deleted,
            (None, _) => ChangeKind::Updated,
        },
        diff: diff.into(),
    })
}
/// OMP times a command against a monotonic clock, so its wall time arrives as a
/// fractional count of milliseconds.
fn duration(details: &Value) -> Option<u64> {
    details["wallTimeMs"]
        .as_f64()
        .filter(|millis| millis.is_finite() && *millis >= 0.0)
        .map(|millis| millis.round() as u64)
}
/// The phase OMP recorded for a block of assistant text.
///
/// A text block carries the provider item it came from in `textSignature`, and
/// for the models that separate a turn's narration from its answer that
/// signature is a versioned JSON object holding the provider's own phase.
/// Models that sign text with an opaque blob, and models that do not sign text
/// at all, say nothing about the phase, and an unstated phase is the answer.
fn phase(block: &Value) -> Phase {
    let signature = text(block, "textSignature");
    if !signature.starts_with('{') {
        return Phase::Final;
    }
    let Ok(signature) = serde_json::from_str::<Value>(signature) else {
        return Phase::Final;
    };
    if signature["v"].as_u64() != Some(1) || text(&signature, "phase") != "commentary" {
        return Phase::Final;
    }
    Phase::Commentary
}
/// The commands oh-my-pi lists, and the skills it lists among them.
///
/// A skill is listed as the command `skill:name` from the source `skill`.
/// It is one, in that oh-my-pi runs it from anywhere in a message rather than
/// only as the whole of one, so it is offered as a skill wherever the reader
/// is writing.
fn commands(value: &Value) -> [Event; 2] {
    let mut commands = Vec::new();
    let mut skills = Vec::new();
    for command in value["commands"].as_array().into_iter().flatten() {
        let name = text(command, "name");
        let description = text(command, "description").into();
        match name.strip_prefix("skill:") {
            Some(skill) if command["source"] == "skill" && !skill.is_empty() => {
                skills.push(chat::Skill {
                    name: skill.into(),
                    description,
                    invocation: format!("/{name}"),
                });
            }
            _ => commands.push(chat::SlashCommand {
                name: name.into(),
                description,
            }),
        }
    }
    [Event::Commands(commands), Event::Skills(skills)]
}
fn answer_value(answers: &[Answer]) -> String {
    answers
        .iter()
        .find(|answer| answer.field == "value")
        .and_then(|answer| answer.values.first())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// oh-my-pi lists a skill as the command `skill:name`, which it runs from
    /// anywhere in a message, and loads only the first a message names.
    #[test]
    fn skills_are_listed_apart_from_commands_and_load_first_only() {
        let events = commands(&json!({"commands":[
            {"name":"compact","description":"Compact","source":"builtin"},
            {"name":"skill:review","description":"Review","source":"skill"},
            {"name":"skill:odd","description":"Not a skill","source":"prompt"},
        ]}));
        assert!(matches!(&events[0], Event::Commands(commands)
            if commands.iter().map(|c| c.name.as_str()).eq(["compact", "skill:odd"])));
        assert!(matches!(&events[1], Event::Skills(skills)
            if skills.len() == 1 && skills[0].name == "review" && skills[0].invocation == "/skill:review"));
    }

    #[test]
    fn usage_refresh_keeps_context_and_cumulative_totals_together() {
        let mut adapter = Omp::default();
        adapter.receive(json!({"type":"response","command":"get_session_stats","success":true,"data":{"tokens":{"input":100,"output":17},"cost":0.01}})).unwrap();
        let request = adapter.request("get_state");
        let step = adapter.receive(json!({"type":"response","id":request["id"],"command":"get_state","success":true,"data":{"sessionId":"session","contextUsage":{"tokens":400,"contextWindow":1000}}})).unwrap();
        assert!(step.events.iter().any(|event| matches!(event, Event::Usage(usage)
            if usage.input_tokens == 100 && usage.output_tokens == 17 && usage.context_tokens == 400)));
        let step = adapter.receive(json!({"type":"response","command":"get_session_stats","success":true,"data":{"tokens":{"input":120,"output":23},"cost":0.02}})).unwrap();
        assert!(step.events.iter().any(|event| matches!(event, Event::Usage(usage)
            if usage.output_tokens == 23 && usage.context_tokens == 400 && usage.context_limit == 1000)));
    }

    /// The heading the surface draws over a conversation is the conversation's
    /// own name. A reader saw `irc` there, because the harness had labelled
    /// the session with the kind of event it was carrying.
    #[test]
    fn a_session_label_that_is_an_event_kind_is_not_a_heading() {
        let titled = |name: Value| {
            let mut adapter = Omp::default();
            let request = adapter.request("get_state");
            let step = adapter
                .receive(json!({"type":"response","id":request["id"],"command":"get_state","success":true,
                    "data":{"sessionId":"session","sessionName":name}}))
                .unwrap();
            step.events.iter().find_map(|event| match event {
                Event::Session { title, .. } => Some(title.clone()),
                _ => None,
            })
        };

        for label in ["irc", "system_notice", "#general", "notify:3", ""] {
            assert_eq!(
                titled(json!(label)),
                Some(None),
                "{label:?} is an internal label, not the name of a conversation"
            );
        }
        assert_eq!(
            titled(json!("Rework the status line")),
            Some(Some("Rework the status line".into())),
            "a conversation the reader named keeps its heading"
        );
        assert_eq!(titled(Value::Null), Some(None));
    }

    /// Compaction's only visible effect is the context figure, so the reply
    /// to one has to ask for the session's state again. The surface draws
    /// that figure as a ring beside the composer's compact control, and a
    /// control whose effect never arrives reads as a control that does
    /// nothing.
    #[test]
    fn a_compaction_reply_asks_for_the_session_state_again() {
        let mut adapter = Omp {
            ready: true,
            ..Omp::default()
        };
        let requested = adapter.command(Command::Compact).unwrap();
        assert_eq!(requested.writes[0]["type"], "compact");
        let step = adapter
            .receive(json!({"type":"response","id":requested.writes[0]["id"],"command":"compact","success":true,"data":{}}))
            .unwrap();
        let kinds = step
            .writes
            .iter()
            .map(|frame| frame["type"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>();
        for expected in ["get_state", "get_session_stats"] {
            assert!(
                kinds.contains(&expected.to_owned()),
                "the compacted session's {expected} is never asked for: {kinds:?}"
            );
        }
    }

    #[test]
    fn exact_resume_refuses_a_different_conversation() {
        let mut adapter = Omp {
            expected_session: Some("expected".into()),
            ..Default::default()
        };
        let request = adapter.request("get_state");
        assert!(adapter.receive(json!({"type":"response","id":request["id"],"command":"get_state","success":true,"data":{"sessionId":"other"}})).is_err());
        assert!(!adapter.ready);
    }

    #[test]
    fn cancelled_interaction_cannot_be_answered_or_approved_late() {
        let mut adapter = Omp {
            ready: true,
            ..Default::default()
        };
        let request = adapter.receive(json!({"type":"extension_ui_request","id":"permission","method":"confirm","title":"Allow write?","message":"src/main.rs"})).unwrap();
        assert!(request.writes.is_empty());
        assert!(
            matches!(&request.events[0], Event::Request(request) if request.actions.iter().any(|choice|choice.id=="cancel"))
        );
        adapter.receive(json!({"type":"extension_ui_request","id":"cancel-notice","method":"cancel","targetId":"permission"})).unwrap();
        assert!(
            adapter
                .command(Command::Answer {
                    request_id: "permission".into(),
                    action: "submit".into(),
                    answers: Vec::new()
                })
                .is_err()
        );
    }

    #[test]
    fn streamed_and_completed_text_share_identity_and_commands_are_visible() {
        let mut adapter = Omp::default();
        let delta = adapter.receive(json!({"type":"message_update","message":{"role":"assistant","timestamp":123},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"hello"}})).unwrap();
        let end = adapter.receive(json!({"type":"message_end","message":{"role":"assistant","timestamp":123,"content":[{"type":"text","text":"hello"}]}})).unwrap();
        let Event::TextDelta { id, .. } = &delta.events[0] else {
            panic!("delta")
        };
        assert!(matches!(&end.events[0],Event::Item(item) if &item.id==id));
        let command = adapter
            .receive(json!({"type":"command_output","text":"Current model: example"}))
            .unwrap();
        assert!(
            matches!(&command.events[0],Event::Item(Item{content:ItemContent::Text{text,..},..}) if text=="Current model: example")
        );
    }

    fn state_reply(request: &Value, id: &str) -> Value {
        json!({"type":"response","id":request["id"],"command":"get_state","success":true,"data":{"sessionId":id,"isStreaming":false,"isCompacting":false}})
    }

    fn page_reply(request: &Value, message: &str, cursor: Option<&str>) -> Value {
        json!({"type":"response","id":request["id"],"command":"get_messages_page","success":true,"data":{"messages":[{"role":"user","timestamp":1,"content":message}],"nextCursor":cursor,"totalMessages":if cursor.is_some(){2}else{1}}})
    }

    #[test]
    fn asynchronous_local_session_switch_reloads_history_and_rejects_old_pages() {
        let mut adapter = Omp {
            session_id: Some("old".into()),
            ..Default::default()
        };
        let initial = adapter.request("get_state");
        let initial = adapter.receive(state_reply(&initial, "old")).unwrap();
        let first_page = initial
            .writes
            .iter()
            .find(|request| request["type"] == "get_messages_page")
            .unwrap();
        let old = adapter
            .receive(page_reply(
                first_page,
                "Old conversation",
                Some("old-cursor"),
            ))
            .unwrap();
        let stale_page = &old.writes[0];
        assert_eq!(stale_page["cursor"], "old-cursor");
        adapter.receive(json!({"type":"extension_ui_request","id":"old-permission","method":"confirm","title":"Allow?"})).unwrap();
        let acknowledged = adapter
            .receive(json!({"type":"response","id":"slash","command":"prompt","success":true}))
            .unwrap();
        assert!(acknowledged.writes.is_empty());
        let completed = adapter
            .receive(json!({"type":"prompt_result","id":"slash","agentInvoked":false}))
            .unwrap();
        let state = completed
            .writes
            .iter()
            .find(|request| request["type"] == "get_state")
            .unwrap();
        let switched = adapter.receive(state_reply(state, "new")).unwrap();
        assert!(matches!(&switched.events[0], Event::Reset { .. }));
        assert!(
            switched
                .events
                .iter()
                .any(|event| matches!(event,Event::Session {id,..} if id=="new"))
        );
        assert_eq!(adapter.session_id.as_deref(), Some("new"));
        let page = switched
            .writes
            .iter()
            .find(|request| request["type"] == "get_messages_page")
            .unwrap();
        assert!(page.get("cursor").is_none());
        let stale = adapter
            .receive(page_reply(
                stale_page,
                "Stale conversation",
                Some("stale-next"),
            ))
            .unwrap();
        assert!(stale.events.is_empty());
        assert!(stale.writes.is_empty());
        assert!(!adapter.ready);
        let loaded = adapter
            .receive(page_reply(page, "New conversation", Some("new-cursor")))
            .unwrap();
        assert!(loaded.events.iter().any(|event|matches!(event,Event::Item(Item {content:ItemContent::Text {text,..},..}) if text=="New conversation")));
        assert_eq!(loaded.writes[0]["cursor"], "new-cursor");
        let finished = adapter
            .receive(page_reply(&loaded.writes[0], "Second page", None))
            .unwrap();
        assert!(
            finished
                .events
                .iter()
                .any(|event| matches!(event, Event::Status { busy: false, .. }))
        );
        assert!(adapter.ready);
        // A sender waits for this rather than for a refusal.
        assert!(finished.events.contains(&Event::Ready(true)));
        assert!(
            adapter
                .command(Command::Answer {
                    request_id: "old-permission".into(),
                    action: "submit".into(),
                    answers: Vec::new()
                })
                .is_err()
        );
    }

    #[test]
    fn stale_state_reply_cannot_restore_old_cli_identity() {
        let mut adapter = Omp {
            ready: true,
            session_id: Some("old".into()),
            ..Default::default()
        };
        let previous = adapter.request("get_state");
        let completion = adapter
            .receive(json!({"type":"prompt_result","id":"slash","agentInvoked":false}))
            .unwrap();
        let current = completion
            .writes
            .iter()
            .find(|request| request["type"] == "get_state")
            .unwrap();
        adapter.receive(state_reply(current, "new")).unwrap();
        let stale = adapter.receive(state_reply(&previous, "old")).unwrap();
        assert!(stale.events.is_empty());
        assert!(stale.writes.is_empty());
        assert_eq!(adapter.session_id.as_deref(), Some("new"));
    }

    #[test]
    fn local_command_preserves_same_session_history_and_pending_permission() {
        let mut adapter = Omp {
            ready: true,
            session_id: Some("same".into()),
            ..Default::default()
        };
        adapter.receive(json!({"type":"extension_ui_request","id":"permission","method":"confirm","title":"Allow?"})).unwrap();
        let completion = adapter
            .receive(json!({"type":"prompt_result","id":"slash","agentInvoked":false}))
            .unwrap();
        let state = completion
            .writes
            .iter()
            .find(|request| request["type"] == "get_state")
            .unwrap();
        let unchanged = adapter.receive(state_reply(state, "same")).unwrap();
        assert!(
            !unchanged
                .events
                .iter()
                .any(|event| matches!(event, Event::Reset { .. }))
        );
        assert!(
            !unchanged
                .writes
                .iter()
                .any(|request| request["type"] == "get_messages_page")
        );
        let answer = adapter
            .command(Command::Answer {
                request_id: "permission".into(),
                action: "submit".into(),
                answers: Vec::new(),
            })
            .unwrap();
        assert_eq!(
            answer.writes,
            vec![json!({"type":"extension_ui_response","id":"permission","confirmed":true})]
        );
        let failure = adapter.receive(json!({"type":"response","id":"slash","command":"prompt","success":false,"error":"Extension failed"})).unwrap();
        assert!(
            matches!(&failure.events[0],Event::Error {message,fatal:false} if message=="Extension failed")
        );
    }

    /// A reply the conversation needs before it takes commands that fails
    /// ends it, since it could never be ready; once ready, the same failure
    /// is only reported.
    #[test]
    fn a_failed_start_ends_the_conversation() {
        for ready in [false, true] {
            let mut adapter = Omp {
                ready,
                state_request: Some("state".into()),
                ..Default::default()
            };
            let failure = adapter
                .receive(json!({"type":"response","id":"state","command":"get_state","success":false,"error":"No session"}))
                .unwrap();
            assert!(
                matches!(&failure.events[0], Event::Error { message, fatal } if message == "No session" && *fatal != ready),
                "{:?}",
                failure.events
            );
        }
    }

    #[test]
    fn a_shell_tool_keeps_the_command_it_ran_once_only_its_result_is_left() {
        let mut adapter = Omp::default();
        let started = adapter.receive(json!({"type":"tool_execution_start","toolCallId":"call","toolName":"bash","args":{"command":"cargo check","cwd":"/repo"},"intent":"Checking the crate"})).unwrap();
        assert!(matches!(&started.events[0],
            Event::Item(Item { content: ItemContent::Command { command, working_directory, action: CommandAction::Run, status: ToolStatus::Running, output, .. }, .. })
            if command == "cargo check" && working_directory == "/repo" && output.is_empty()));
        let finished = adapter.receive(json!({"type":"tool_execution_end","toolCallId":"call","toolName":"bash","isError":true,"result":{"content":[{"type":"text","text":"error[E0425]"}],"details":{"exitCode":101,"wallTimeMs":1520.5}}})).unwrap();
        let Event::Item(item) = &finished.events[0] else {
            panic!("item")
        };
        assert_eq!(item.id, "tool:call");
        assert!(matches!(&item.content,
            ItemContent::Command { label, command, working_directory, action: CommandAction::Run, status: ToolStatus::Failed, exit_code: Some(101), duration_ms: Some(1521), output }
            if label.is_empty() && command == "cargo check" && working_directory == "/repo" && output == "error[E0425]"));
        // The same completion arrives again as the tool-result message, which
        // must not degrade the step it already produced.
        let repeated = adapter.receive(json!({"type":"message_end","message":{"role":"toolResult","toolCallId":"call","toolName":"bash","content":[{"type":"text","text":"error[E0425]"}],"isError":true,"details":{"exitCode":101,"wallTimeMs":1520.5}}})).unwrap();
        assert!(matches!(&repeated.events[0],
            Event::Item(Item { content: ItemContent::Command { command, .. }, .. }) if command == "cargo check"));
    }

    #[test]
    fn an_argument_free_update_cannot_erase_the_command_a_call_started_with() {
        let mut adapter = Omp::default();
        adapter.receive(json!({"type":"tool_execution_start","toolCallId":"call","toolName":"bash","args":{"command":"cargo check","cwd":"/repo"}})).unwrap();
        let progressed = adapter.receive(json!({"type":"tool_execution_update","toolCallId":"call","toolName":"bash","partialResult":{"content":[{"type":"text","text":"Checking cagents"}]}})).unwrap();
        assert!(matches!(&progressed.events[0],
            Event::Item(Item { content: ItemContent::Command { command, working_directory, status: ToolStatus::Running, output, .. }, .. })
            if command == "cargo check" && working_directory == "/repo" && output == "Checking cagents"));
        let finished = adapter.receive(json!({"type":"tool_execution_end","toolCallId":"call","toolName":"bash","result":{"content":[{"type":"text","text":"Finished"}],"details":{"exitCode":0}}})).unwrap();
        assert!(matches!(&finished.events[0],
            Event::Item(Item { content: ItemContent::Command { command, working_directory, status: ToolStatus::Succeeded, .. }, .. })
            if command == "cargo check" && working_directory == "/repo"));
    }

    #[test]
    fn a_long_session_stops_carrying_the_commands_of_its_oldest_calls() {
        let mut adapter = Omp::default();
        for index in 0..=MAX_CALLS {
            adapter.receive(json!({"type":"tool_execution_start","toolCallId":format!("call-{index}"),"toolName":"bash","args":{"command":format!("echo {index}"),"cwd":"/repo"}})).unwrap();
        }
        let evicted = adapter.receive(json!({"type":"tool_execution_end","toolCallId":"call-0","toolName":"bash","result":{"content":[{"type":"text","text":"0"}],"details":{"exitCode":0}}})).unwrap();
        assert!(matches!(&evicted.events[0],
            Event::Item(Item { content: ItemContent::Tool { name, .. }, .. }) if name == "bash"));
        let newest = adapter.receive(json!({"type":"tool_execution_end","toolCallId":format!("call-{MAX_CALLS}"),"toolName":"bash","result":{"content":[{"type":"text","text":"done"}],"details":{"exitCode":0}}})).unwrap();
        assert!(matches!(&newest.events[0],
            Event::Item(Item { content: ItemContent::Command { command, .. }, .. })
            if command == &format!("echo {MAX_CALLS}")));
    }

    #[test]
    fn a_moved_file_reports_the_path_it_came_from_rather_than_an_edit() {
        let adapter = Omp::default();
        let content = adapter.result(
            "call",
            "edit",
            "Renamed src/old.rs".into(),
            &json!({"path":"src/new.rs","sourcePath":"src/old.rs","op":"update","diff":"@@ -1 +1 @@\n-old\n+new"}),
            ToolStatus::Succeeded,
        );
        let ItemContent::Changes { changes, .. } = content else {
            panic!("changes")
        };
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "src/new.rs");
        assert_eq!(
            changes[0].kind,
            ChangeKind::Renamed {
                from: "src/old.rs".into()
            }
        );
        assert_eq!(changes[0].diff, "@@ -1 +1 @@\n-old\n+new");
    }

    /// The exact launch a live installation persisted for a native OMP card:
    /// a session directory, no other options, and a UUIDv7 conversation. The
    /// rebuild has to carry that conversation into the resumed argv, because a
    /// restore after a daemon restart has nothing else to put the card back on
    /// the transcript it was showing.
    #[test]
    fn the_persisted_native_recipe_rebuilds_into_an_exact_resume() {
        use crate::{agent::Provider, model::LaunchRecipe};
        const CONVERSATION: &str = "01a0ab66-21d6-7065-9fe5-95f302b669e1";
        let launch = Launch {
            provider: Provider::Omp,
            recipe: LaunchRecipe {
                program: "omp".into(),
                arguments: vec![
                    "--session-dir".into(),
                    "/home/user/.omp/agent/sessions/native".into(),
                ],
                working_directory: "/home/user".into(),
                environment: Default::default(),
            },
            session_id: Some(CONVERSATION.into()),
        };
        let (spec, _) = create(&launch).expect("the persisted native recipe is resumable");
        assert_eq!(
            spec.arguments,
            [
                "--session-dir",
                "/home/user/.omp/agent/sessions/native",
                "--mode",
                "rpc-ui",
                "--resume",
                CONVERSATION,
            ]
        );
    }
}
