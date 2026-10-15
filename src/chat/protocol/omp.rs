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
    /// Whether the conversation has ever been ready. A conversation that
    /// changed sessions is not ready again until the new one loads, but it
    /// is running and is not ended for a reply that failed meanwhile.
    started: bool,
    busy: bool,
    turn_outcome: Option<crate::notifications::Kind>,
    agent_turn_active: bool,
    expected_session: Option<String>,
    session_id: Option<String>,
    state_request: Option<String>,
    history_request: Option<String>,
    requests: HashMap<String, String>,
    chunks: HashMap<String, Chunk>,
    dropped_chunks: HashMap<String, (usize, usize)>,
    calls: HashMap<String, Call>,
    call_order: VecDeque<String>,
    usage: chat::Usage,
    execution: Option<Execution>,
    children: HashMap<String, Box<Omp>>,
    child_requests: HashMap<String, String>,
    subscribed: bool,
    subscription_request: Option<String>,
    children_request: Option<String>,
    child_loaded: std::collections::HashSet<String>,
    child_buffer: HashMap<String, Vec<Value>>,
    child_finished: VecDeque<String>,
    branch_draft: Option<String>,
    branch_request: Option<String>,
    configured_reasoning: Option<String>,
}
struct Execution {
    run: crate::model::RunId,
    request: String,
    accepted: bool,
    acknowledged_start: bool,
    observing: bool,
    settled: bool,
    usage_before: chat::Usage,
    session: Option<String>,
    /// Older OMP builds emit a terminal agent_end but no correlated result.
    /// Confirm its known outcome with the idle state requested after that end.
    completion_probe: Option<(String, crate::notifications::Kind)>,
    completion_outcome: Option<crate::notifications::Kind>,
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
            // Only a shell command writes for a terminal; a file that was
            // read is shown as the bytes it holds.
            output: match self.action {
                CommandAction::Run => super::terminal_output::display(output),
                _ => output,
            },
        }
    }
}
impl Omp {
    fn finish_execution(
        &mut self,
        request: &str,
        outcome: crate::notifications::Kind,
        step: &mut Step,
    ) {
        if !self
            .execution
            .as_ref()
            .is_some_and(|execution| execution.request == request)
        {
            return;
        }
        let execution = self.execution.take().unwrap();
        use crate::agent_execution::ProviderEvent;
        if !execution.settled {
            step.events.push(Event::Execution {
                run: execution.run,
                event: ProviderEvent::Settled,
            });
        }
        step.events.push(Event::Execution {
            run: execution.run,
            event: ProviderEvent::Finished { outcome },
        });
        step.events.push(Event::TurnFinished {
            key: Some(format!("omp-request:{request}")),
            outcome,
        });
    }

    fn request(&mut self, kind: &str) -> Value {
        self.sequence = self.sequence.wrapping_add(1);
        let id = format!("cagents-{}", self.sequence);
        if kind == "get_state" {
            self.state_request = Some(id.clone());
            if let Some((probe, _)) = self
                .execution
                .as_mut()
                .and_then(|execution| execution.completion_probe.as_mut())
            {
                *probe = id.clone();
            }
        }
        if kind == "get_branch_messages" {
            self.branch_request = Some(id.clone());
        }
        json!({"id":id,"type":kind})
    }
    fn refresh(&mut self) -> Step {
        let mut step = Step::default();
        if !self.subscribed && self.subscription_request.is_none() {
            let mut subscription = self.request("set_subagent_subscription");
            subscription["level"] = "events".into();
            self.subscription_request = Some(text(&subscription, "id").into());
            step.writes.push(subscription);
        }
        for kind in [
            "get_state",
            "get_available_models",
            "get_available_commands",
            "get_session_stats",
        ] {
            step.writes.push(self.request(kind));
        }
        if self.children_request.is_none() {
            let request = self.request("get_subagents");
            self.children_request = Some(text(&request, "id").into());
            step.writes.push(request);
        }
        step
    }
    fn child(&mut self, id: &str) -> Option<&mut Omp> {
        if !self.children.contains_key(id)
            && self.children.len() >= 32
            && let Some(old) = self.child_finished.pop_front()
        {
            self.children.remove(&old);
            self.child_loaded.remove(&old);
            self.child_buffer.remove(&old);
            self.child_requests.retain(|_, id| id != &old);
        }
        if id.is_empty()
            || id.len() > 256
            || (!self.children.contains_key(id) && self.children.len() >= 32)
        {
            return None;
        }
        Some(self.children.entry(id.into()).or_default())
    }
    fn read_child(&mut self, id: &str, step: &mut Step) {
        if self.child(id).is_none()
            || self.child_loaded.contains(id)
            || self.child_requests.values().any(|child| child == id)
        {
            return;
        }
        let mut request = self.request("get_subagent_messages");
        request["subagentId"] = id.into();
        self.child_requests
            .insert(text(&request, "id").into(), id.into());
        self.child_buffer.insert(id.into(), Vec::new());
        step.writes.push(request);
    }
    fn child_metadata(&mut self, payload: &Value, step: &mut Step) {
        let progress = if payload["progress"].is_object() {
            &payload["progress"]
        } else {
            payload
        };
        let id = text(progress, "id");
        if self.child(id).is_none() {
            return;
        }
        let status = match text(progress, "status") {
            "completed" => ToolStatus::Succeeded,
            "failed" => ToolStatus::Failed,
            "aborted" => ToolStatus::Stopped,
            _ => ToolStatus::Running,
        };
        if status == ToolStatus::Running {
            self.child_finished.retain(|finished| finished != id);
        } else if status != ToolStatus::Running
            && !self.child_finished.iter().any(|finished| finished == id)
        {
            self.child_finished.push_back(id.into());
        }
        step.events.push(Event::Subagent {
            model: None,
            waiting: None,
            detail: None,
            id: id.into(),
            kind: text(payload, "agent").into(),
            description: payload["task"]
                .as_str()
                .or_else(|| payload["description"].as_str())
                .or_else(|| progress["description"].as_str())
                .unwrap_or_default()
                .into(),
            status: Some(status),
            // A subagent run as a background job carries on past the turn,
            // and past `abort`, until its job is cancelled.
            background: payload["detached"].as_bool(),
        });
        self.read_child(id, step);
    }
    fn child_content(&mut self, frame: Value) -> Result<Step, String> {
        // The subscription includes the authoritative partial message. Reapply
        // that full item after history hydration instead of appending a delta
        // that may already be included in the snapshot.
        if text(&frame, "type") == "message_update"
            && frame["assistantMessageEvent"]["partial"].is_object()
        {
            return Ok(Step {
                events: self
                    .message_items(&frame["assistantMessageEvent"]["partial"])
                    .into_iter()
                    .map(Event::Item)
                    .collect(),
                ..Step::default()
            });
        }
        self.receive(frame)
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
                started_at: None,
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
                started_at: None,
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
                    started_at: None,
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
                    kind: crate::chat::RequestKind::Question,
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
                    started_at: None,
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
                    started_at: None,
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
    fn command_for_run(
        &mut self,
        run: crate::model::RunId,
        command: Command,
    ) -> Result<Step, String> {
        if self.busy
            || self.execution.is_some()
            || !matches!(
                &command,
                Command::Prompt {
                    mode: PromptMode::Send,
                    ..
                }
            )
        {
            return Err("A correlated OMP run requires one idle direct prompt".into());
        }
        let mut step = self.command(command)?;
        let request = step
            .writes
            .iter()
            .find(|frame| frame["type"] == "prompt")
            .and_then(|frame| frame["id"].as_str())
            .ok_or("OMP did not create the prompt request")?
            .to_owned();
        self.execution = Some(Execution {
            run,
            request: request.clone(),
            accepted: false,
            acknowledged_start: false,
            observing: false,
            settled: false,
            usage_before: self.usage.clone(),
            session: self.session_id.clone(),
            completion_probe: None,
            completion_outcome: None,
        });
        step.events.insert(
            0,
            Event::Execution {
                run,
                event: crate::agent_execution::ProviderEvent::Started {
                    key: format!("omp-request:{request}"),
                },
            },
        );
        Ok(step)
    }
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
                if self.busy && mode == PromptMode::Send {
                    return Err("OMP is busy; steer or queue instead of sending another prompt".into());
                }
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
            // OMP cancels one background job only through the model's own
            // `hub` tool; its RPC offers no command that stops one subagent.
            Command::StopSubagent { .. } => return Err("This OMP protocol cannot stop a single subagent".into()),
            Command::SetInterruptMode { mode } => {
                if !matches!(mode.as_str(), "immediate" | "wait") {
                    return Err("Choose immediate or wait for OMP steering".into());
                }
                let mut request = self.request("set_interrupt_mode");
                request["mode"] = mode.into();
                step.writes.push(request);
            }
            Command::RemoveQueued { .. } | Command::StartQueued { .. } => return Err("This OMP protocol does not support queue mutation".into()),
            Command::ListBranches => step.writes.push(self.request("get_branch_messages")),
            Command::Branch { id } => {
                if self.busy { return Err("Branch requires an idle OMP conversation".into()); }
                let mut request = self.request("branch");
                request["entryId"] = id.ok_or("Choose the prompt to branch before")?.into();
                step.writes.push(request);
            }
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
            Command::SetFastMode { .. } => return Err("Fast mode is not advertised by this OMP connection".into()),
            Command::SetPermissionMode{..} => return Err("OMP approval policy is configured at launch; use its settings command to change policy.".into()),
            Command::Compact => step.writes.push(self.request("compact")),
            Command::ResumeAfterLogin { .. } => return Err("Live login verification is available for Codex chats".into()),
            Command::Refresh => return Ok(self.refresh()),
            Command::LoadFileChanges { .. } | Command::LoadFileChangePatches { .. } | Command::LoadOriginal { .. } | Command::PromptOriginal { .. } | Command::PromptNamed { .. } =>
                return Err("File changes are served by the transcript cache".into()),
            Command::LoadLater => return Err("Newer history is served by the transcript cache".into()),
            Command::LoadEarlier => return Err("OMP history is loaded completely during connection.".into()),
            Command::DismissPermissionDenial { tool_use_id } => step.events.push(Event::PermissionDenialDismissed { tool_use_id }),
            Command::RefreshPermissions | Command::UpdatePermissionSettings { .. } => return Err("Permission rule settings are available for Claude sessions".into()),
            Command::Slash{command} => {let mut request=self.request("prompt");request["message"]=command.into();step.writes.push(request);}
        }
        Ok(step)
    }
    fn receive(&mut self, frame: Value) -> Result<Step, String> {
        let mut step = Step::default();
        let execution_scope = self
            .execution
            .as_ref()
            .filter(|execution| execution.observing && !execution.settled)
            .map(|execution| execution.run);
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
                if let Some((expected, next)) = self.dropped_chunks.get_mut(&id) {
                    if *expected != count || *next != index {
                        return Err("Out-of-order OMP child history chunks".into());
                    }
                    *next += 1;
                    if *next == count {
                        self.dropped_chunks.remove(&id);
                    }
                    return Ok(step);
                }
                if index == 0
                    && (bytes > MAX_PAYLOAD || count > 1024)
                    && count <= 1_000_000
                    && self.dropped_chunks.len() < 4
                {
                    let decoded = STANDARD
                        .decode(text(&frame, "data"))
                        .map_err(|error| error.to_string())?;
                    // Chunk zero contains the response envelope before its data.
                    // Authenticate that request ID before discarding an optional
                    // transcript; mandatory parent replies still fail normally.
                    let envelope = decoded
                        .windows(b"\"data\":".len())
                        .position(|bytes| bytes == b"\"data\":")
                        .and_then(|end| std::str::from_utf8(&decoded[..end]).ok())
                        .and_then(|prefix| {
                            serde_json::from_str::<Value>(&format!("{prefix}\"data\":null}}")).ok()
                        });
                    if let Some(envelope) = envelope
                        && envelope["command"] == "get_subagent_messages"
                        && let Some(child_id) = self.child_requests.remove(text(&envelope, "id"))
                    {
                        if count > 1 {
                            self.dropped_chunks.insert(id, (count, 1));
                        }
                        self.child_loaded.insert(child_id.clone());
                        for event in self.child_buffer.remove(&child_id).unwrap_or_default() {
                            if let Some(child) = self.children.get_mut(&child_id) {
                                let content = child.child_content(event)?;
                                step.events
                                    .extend(chat::child_events(&child_id, content.events));
                            }
                        }
                        step.events.push(Event::Subagent {
                            model: None, waiting: None, detail: None,
                            id: child_id, kind: String::new(),
                            description: "Earlier child history exceeds the display limit; live activity remains available".into(), status: None, background: None,
                        });
                        return Ok(step);
                    }
                }
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
                if text(&frame, "command") == "get_subagent_messages" {
                    let Some(id) = self.child_requests.remove(text(&frame, "id")) else {
                        return Ok(step);
                    };
                    if frame["success"] == true {
                        self.child_loaded.insert(id.clone());
                    }
                    if let Some(child) = self.children.get_mut(&id) {
                        for message in frame["data"]["messages"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|_| frame["success"].as_bool() == Some(true))
                        {
                            for item in child.message_items(message) {
                                step.events.push(Event::SubagentItem {
                                    id: id.clone(),
                                    item,
                                });
                            }
                        }
                        for event in self.child_buffer.remove(&id).unwrap_or_default() {
                            let content = child.child_content(event)?;
                            step.events.extend(chat::child_events(&id, content.events));
                        }
                    }
                    return Ok(step);
                }
                // Replies may arrive after a local extension has switched sessions.
                // Only the current state request and current page may mutate the view.
                let pending = match text(&frame, "command") {
                    "get_state" => Some(&mut self.state_request),
                    "get_messages_page" => Some(&mut self.history_request),
                    "get_subagents" => Some(&mut self.children_request),
                    "set_subagent_subscription" => Some(&mut self.subscription_request),
                    "get_branch_messages" => Some(&mut self.branch_request),
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
                    let fatal = !self.started
                        && matches!(
                            text(&frame, "command"),
                            "negotiate_protocol" | "get_state" | "get_messages_page"
                        );
                    step.events.push(Event::Error {
                        message: text(&frame, "error").into(),
                        fatal,
                    });
                    // A refused prompt started no turn, so the conversation is
                    // idle again rather than waiting for one.
                    if text(&frame, "command") == "prompt" && !self.busy {
                        step.events.push(Event::Status {
                            busy: false,
                            detail: "Ready".into(),
                        });
                    }
                    self.finish_execution(
                        text(&frame, "id"),
                        crate::notifications::Kind::Failed,
                        &mut step,
                    );
                    return Ok(step);
                }
                let data = &frame["data"];
                if text(&frame, "command") == "prompt"
                    && let Some(execution) = self.execution.as_mut().filter(|execution| {
                        frame["id"].as_str() == Some(execution.request.as_str())
                    })
                {
                    execution.accepted = true;
                }
                match text(&frame, "command") {
                    "negotiate_protocol" => step = self.refresh(),
                    "follow_up" => {
                        step.writes.push(self.request("get_state"));
                    }
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
                            self.children.clear();
                            self.child_requests.clear();
                            self.child_loaded.clear();
                            self.child_buffer.clear();
                            self.child_finished.clear();
                            self.children_request = None;
                            self.branch_request = None;
                            self.configured_reasoning = None;
                            self.usage = chat::Usage::default();
                            step.events.push(Event::Reset {
                                reason: "OMP changed the current conversation.".into(),
                            });
                            step.events.push(Event::Ready(false));
                        }
                        self.session_id = Some(id.into());
                        if let Some(text) = self.branch_draft.take() {
                            step.events.push(Event::Composer { text });
                        }
                        if let Some(mode) = data["interruptMode"].as_str() {
                            step.events.push(Event::InterruptMode(mode.into()));
                        }
                        if let Some(messages) = data["queuedMessages"]["followUp"].as_array() {
                            step.events.push(Event::QueueChanged(
                                messages
                                    .iter()
                                    .take(chat::QueuedMessage::LIMIT)
                                    .enumerate()
                                    .filter_map(|(index, message)| {
                                        Some(chat::QueuedMessage::new(
                                            format!("omp-followup-{index}"),
                                            message.as_str()?.to_owned(),
                                        ))
                                    })
                                    .collect(),
                            ));
                        }
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
                            queue: data["queuedMessages"]["followUp"].is_array(),
                            attachments: true,
                            compact: true,
                            model_selection: true,
                            reasoning_selection: true,
                            permission_modes: Vec::new(),
                            skill_reach: chat::SkillReach::First,
                            interrupt_modes: vec!["immediate".into(), "wait".into()],
                            branch: true,
                            ..chat::Capabilities::default()
                        }));
                        step.events.push(Event::Models {
                            models: Vec::new(),
                            selected: model_id(&data["model"]),
                            reasoning: self
                                .configured_reasoning
                                .clone()
                                .or_else(|| data["thinkingLevel"].as_str().map(str::to_owned)),
                        });
                        let usage = &data["contextUsage"];
                        self.usage.context_tokens = usage["tokens"].as_u64().unwrap_or(0);
                        self.usage.context_limit = usage["contextWindow"].as_u64().unwrap_or(0);
                        step.events.push(Event::Usage(self.usage.clone()));
                        if data["isStreaming"].as_bool() == Some(false)
                            && data["isCompacting"].as_bool() == Some(false)
                            && let Some((request, outcome)) =
                                self.execution.as_ref().and_then(|execution| {
                                    let (probe, outcome) = execution.completion_probe.as_ref()?;
                                    (frame["id"].as_str() == Some(probe.as_str())
                                        && execution.session.as_deref() == Some(id))
                                    .then(|| (execution.request.clone(), *outcome))
                                })
                        {
                            self.finish_execution(&request, outcome, &mut step);
                        }
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
                                    reasoning: if model["reasoning"].as_bool() == Some(true) {
                                        ["off", "auto"]
                                            .into_iter()
                                            .map(str::to_owned)
                                            .chain(
                                                model["thinking"]["efforts"]
                                                    .as_array()
                                                    .into_iter()
                                                    .flatten()
                                                    .filter_map(Value::as_str)
                                                    .map(str::to_owned),
                                            )
                                            .collect()
                                    } else {
                                        Vec::new()
                                    },
                                })
                            })
                            .collect();
                        step.events.push(Event::Models {
                            models,
                            selected: None,
                            reasoning: None,
                        });
                    }
                    "get_subagents" => {
                        for agent in data["subagents"]
                            .as_array()
                            .or_else(|| data.as_array())
                            .into_iter()
                            .flatten()
                            .take(32)
                        {
                            self.child_metadata(agent, &mut step);
                        }
                    }
                    "set_subagent_subscription" => {
                        self.subscribed = text(data, "level") == "events"
                    }
                    "set_interrupt_mode" => step.writes.push(self.request("get_state")),
                    "get_branch_messages" => {
                        let points = data["messages"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .rev()
                            .take(128)
                            .filter_map(|point| {
                                Some(chat::BranchPoint {
                                    id: point["entryId"].as_str()?.into(),
                                    label: crate::account::clean_label(point["text"].as_str()?)?,
                                })
                            })
                            .collect();
                        step.events.push(Event::BranchPoints(points));
                    }
                    "branch" => {
                        if data["cancelled"] != true {
                            self.ready = false;
                            step.events.push(Event::Ready(false));
                            self.history_request = None;
                            self.branch_request = None;
                            self.children.clear();
                            self.child_requests.clear();
                            self.child_loaded.clear();
                            self.child_buffer.clear();
                            self.child_finished.clear();
                            self.children_request = None;
                            step.writes.push(self.request("get_state"));
                            self.branch_draft = data["text"].as_str().map(str::to_owned);
                        }
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
                            self.started = true;
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
                        step = self.refresh();
                        self.finish_execution(
                            text(&frame, "id"),
                            crate::notifications::Kind::Succeeded,
                            &mut step,
                        );
                    }
                    _ => {}
                }
            }
            "queue_update" => {
                step.writes.push(self.request("get_state"));
            }
            "message_start" | "message_end" => {
                if text(&frame, "type") == "message_start"
                    && text(&frame["message"], "role") == "user"
                {
                    step.writes.push(self.request("get_state"));
                }
                // Only an assistant stop reason describes the turn outcome;
                // a failed tool can be recovered from in the same turn.
                if text(&frame, "type") == "message_end"
                    && text(&frame["message"], "role") == "assistant"
                {
                    use crate::notifications::Kind;
                    self.turn_outcome = match text(&frame["message"], "stopReason") {
                        "error" => Some(Kind::Failed),
                        "aborted" => Some(Kind::Cancelled),
                        "stop" | "length" => Some(Kind::Succeeded),
                        _ => None,
                    };
                }
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
                    started_at: None,
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
                    started_at: None,
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
                    started_at: None,
                    at: None,
                }));
            }
            "subagent_lifecycle" | "subagent_progress" => {
                self.child_metadata(&frame["payload"], &mut step)
            }
            "subagent_event" => {
                let id = text(&frame["payload"], "id");
                if let Some(buffer) = self.child_buffer.get_mut(id) {
                    if buffer.len() < 128
                        && buffer
                            .iter()
                            .map(|event| event.to_string().len())
                            .sum::<usize>()
                            + frame.to_string().len()
                            <= 512 * 1024
                    {
                        buffer.push(frame["payload"]["event"].clone());
                        return Ok(step);
                    }
                    // Keep live content if a snapshot is too slow. Its reply
                    // will be discarded rather than overwrite newer text.
                    self.child_requests.retain(|_, child| child != id);
                    self.child_loaded.insert(id.into());
                    let buffered = self.child_buffer.remove(id).unwrap_or_default();
                    if let Some(child) = self.children.get_mut(id) {
                        for event in buffered {
                            let content = child.child_content(event)?;
                            step.events.extend(chat::child_events(id, content.events));
                        }
                    }
                }
                if let Some(child) = self.child(id) {
                    let child_step = child.child_content(frame["payload"]["event"].clone())?;
                    step.events
                        .extend(chat::child_events(id, child_step.events));
                }
            }
            "thinking_level_changed" => {
                self.configured_reasoning = frame["configured"]
                    .as_str()
                    .or_else(|| frame["thinkingLevel"].as_str())
                    .map(str::to_owned);
                step.events.push(Event::Models {
                    models: Vec::new(),
                    selected: None,
                    reasoning: self.configured_reasoning.clone(),
                });
            }
            "agent_start" | "auto_compaction_start" => {
                if text(&frame, "type") == "agent_start" {
                    step.events.push(Event::TurnStarted);
                    self.turn_outcome = None;
                    self.agent_turn_active = true;
                    if let Some(execution) = &mut self.execution {
                        // An old idle reply cannot finish a run after a newer
                        // autonomous/extension turn has begun.
                        execution.completion_probe = None;
                    }
                    if let Some(execution) = self
                        .execution
                        .as_mut()
                        .filter(|execution| !execution.settled)
                    {
                        execution.observing = true;
                        execution.acknowledged_start = execution.accepted;
                    }
                }
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
            "agent_end"
                if frame["isTerminal"].as_bool() == Some(false)
                    || frame["willContinue"].as_bool() == Some(true) =>
            {
                // OMP can yield for maintenance or asynchronous delivery and
                // resume the same run. Older runtimes omit this field, so only
                // an explicit false excludes the terminal boundary below.
                self.busy = true;
                step.events.push(Event::Status {
                    busy: true,
                    detail: "Working".into(),
                });
            }
            "agent_end" | "auto_compaction_end" => {
                let finished = text(&frame, "type") == "agent_end" && self.agent_turn_active;
                let outcome = if finished {
                    self.agent_turn_active = false;
                    self.turn_outcome.take()
                } else {
                    None
                };
                self.busy = false;
                step = self.refresh();
                if finished
                    && let Some(execution) = self
                        .execution
                        .as_ref()
                        .filter(|execution| !execution.settled)
                    && let Some(usage) =
                        terminal_run_usage(&execution.usage_before, &frame["messages"])
                {
                    self.usage.input_tokens = usage.input_tokens;
                    self.usage.output_tokens = usage.output_tokens;
                    self.usage.cost_microusd = usage.cost_microusd;
                    step.events.push(Event::Usage(self.usage.clone()));
                }
                if finished {
                    step.events.push(Event::TurnSettled);
                }
                if finished
                    && self.execution.is_none()
                    && let Some(outcome) = outcome
                {
                    step.events.push(Event::TurnFinished { key: None, outcome });
                }
                if finished
                    && let Some(execution) = self
                        .execution
                        .as_mut()
                        .filter(|execution| execution.observing && !execution.settled)
                {
                    execution.settled = true;
                    execution.observing = false;
                    if execution.acknowledged_start
                        && let Some(outcome) = outcome
                    {
                        execution.completion_outcome = Some(outcome);
                    }
                    step.events.push(Event::Execution {
                        run: execution.run,
                        event: crate::agent_execution::ProviderEvent::Settled,
                    });
                }
                if finished
                    && let Some(execution) = self
                        .execution
                        .as_mut()
                        .filter(|execution| execution.settled)
                    && let Some(outcome) = execution.completion_outcome
                    && let Some(probe) = &self.state_request
                {
                    execution.completion_probe = Some((probe.clone(), outcome));
                }
                step.events.push(Event::Status {
                    busy: false,
                    detail: "Ready".into(),
                });
            }
            "extension_ui_request" => return self.extension(&frame),
            "available_commands_update" => step.events.extend(commands(&frame)),
            "prompt_result" => {
                if self.execution.as_ref().is_some_and(|execution| {
                    frame["id"].as_str() == Some(execution.request.as_str())
                }) {
                    let outcome = match frame["status"].as_str() {
                        Some("completed") => crate::notifications::Kind::Succeeded,
                        Some("aborted") => crate::notifications::Kind::Cancelled,
                        Some("error") => crate::notifications::Kind::Failed,
                        None if frame["agentInvoked"].as_bool() == Some(false) => {
                            crate::notifications::Kind::Succeeded
                        }
                        _ => {
                            return Err(
                                "OMP completed an accepted prompt with an unknown outcome".into()
                            );
                        }
                    };
                    if let Some(message) = frame["error"]["message"].as_str() {
                        step.events.push(Event::Error {
                            message: message.into(),
                            fatal: false,
                        });
                    }
                    self.finish_execution(text(&frame, "id"), outcome, &mut step);
                }
                if frame["agentInvoked"].as_bool() == Some(false) {
                    let refresh = self.refresh();
                    step.writes.extend(refresh.writes);
                    step.events.extend(refresh.events);
                }
            }
            _ => {}
        }
        if let Some(run) = execution_scope
            && matches!(
                text(&frame, "type"),
                "message_start" | "message_update" | "message_end" | "agent_end" | "prompt_result"
            )
        {
            step.events.push(Event::Execution {
                run,
                event: crate::agent_execution::ProviderEvent::Scope,
            });
        }
        Ok(step)
    }
}
fn terminal_run_usage(before: &chat::Usage, messages: &Value) -> Option<chat::Usage> {
    let mut usage = before.clone();
    let mut observed = false;
    for message in messages
        .as_array()?
        .iter()
        .filter(|message| message["role"] == "assistant")
    {
        let input = message["usage"]["input"].as_u64()?;
        let output = message["usage"]["output"].as_u64()?;
        observed = true;
        usage.input_tokens = usage.input_tokens.saturating_add(input);
        usage.output_tokens = usage.output_tokens.saturating_add(output);
        usage.cost_microusd = usage.cost_microusd.and_then(|before| {
            let cost = message["usage"]["cost"]["total"].as_f64()?;
            (cost.is_finite() && cost >= 0.0)
                .then(|| before.saturating_add((cost * 1_000_000.0) as u64))
        });
    }
    observed.then_some(usage)
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

    #[test]
    fn branching_restores_the_selected_prompt_after_the_confirmed_session_reset() {
        let mut adapter = Omp {
            ready: true,
            session_id: Some("original".into()),
            ..Default::default()
        };
        assert!(
            adapter
                .command(Command::SetInterruptMode {
                    mode: "unknown".into()
                })
                .is_err()
        );
        let timing = adapter
            .command(Command::SetInterruptMode {
                mode: "wait".into(),
            })
            .unwrap();
        assert_eq!(timing.writes[0]["type"], "set_interrupt_mode");
        assert_eq!(timing.writes[0]["mode"], "wait");
        adapter.busy = true;
        let prompt = |mode| Command::Prompt {
            text: "next".into(),
            attachments: Vec::new(),
            mode,
        };
        assert!(adapter.command(prompt(PromptMode::Send)).is_err());
        assert_eq!(
            adapter.command(prompt(PromptMode::Queue)).unwrap().writes[0]["type"],
            "follow_up"
        );
        assert!(
            adapter
                .command(Command::Branch {
                    id: Some("entry-exact".into())
                })
                .is_err()
        );
        adapter.busy = false;
        let branch = adapter
            .command(Command::Branch {
                id: Some("entry-exact".into()),
            })
            .unwrap();
        assert_eq!(branch.writes[0]["entryId"], "entry-exact");
        let response = adapter.receive(json!({"type":"response","id":branch.writes[0]["id"],"command":"branch","success":true,"data":{"text":"Selected prompt"}})).unwrap();
        assert!(response.events.contains(&Event::Ready(false)));
        assert!(
            !response
                .events
                .iter()
                .any(|event| matches!(event, Event::Composer { .. }))
        );
        let state = adapter.receive(json!({"type":"response","id":response.writes[0]["id"],"command":"get_state","success":true,"data":{"sessionId":"branched","isStreaming":false,"interruptMode":"wait","queuedMessages":{"followUp":[]}}})).unwrap();
        let reset = state
            .events
            .iter()
            .position(|event| matches!(event, Event::Reset { .. }))
            .unwrap();
        let draft = state
            .events
            .iter()
            .position(
                |event| matches!(event, Event::Composer { text } if text == "Selected prompt"),
            )
            .unwrap();
        assert!(reset < draft);
        assert_eq!(adapter.session_id.as_deref(), Some("branched"));
        assert!(state.events.contains(&Event::InterruptMode("wait".into())));
    }

    /// `abort` spares a subagent run as a background job, and OMP's RPC has
    /// no command that stops one, so neither is offered.
    #[test]
    fn a_background_job_is_neither_stopped_by_abort_nor_stoppable_alone() {
        let mut adapter = Omp {
            ready: true,
            busy: true,
            ..Default::default()
        };
        let interrupt = adapter.command(Command::Interrupt).unwrap();
        assert_eq!(interrupt.writes[0]["type"], "abort");
        let started = adapter.receive(json!({"type":"subagent_lifecycle","payload":{"id":"child","status":"started","detached":true,"progress":{"id":"child","status":"running"}}})).unwrap();
        assert!(started.events.iter().any(|event| matches!(event, Event::Subagent { id, background: Some(true), .. } if id == "child")));
        assert!(
            adapter
                .command(Command::StopSubagent { id: "child".into() })
                .is_err()
        );
        let aborted = adapter.receive(json!({"type":"subagent_progress","payload":{"progress":{"id":"child","status":"aborted"}}})).unwrap();
        assert!(aborted.events.iter().any(|event| matches!(event, Event::Subagent { id, status: Some(ToolStatus::Stopped), .. } if id == "child")));
    }

    #[test]
    fn oversized_optional_child_history_preserves_parent_control() {
        let mut adapter = Omp {
            ready: true,
            busy: true,
            ..Default::default()
        };
        let metadata = adapter.receive(json!({"type":"subagent_lifecycle","payload":{"progress":{"id":"child","status":"running"},"task":"review"}})).unwrap();
        let request_id = metadata.writes[0]["id"].as_str().unwrap();
        let prefix = format!(
            "{{\"type\":\"response\",\"id\":\"{request_id}\",\"command\":\"get_subagent_messages\",\"success\":true,\"data\":{{"
        );
        // Chunks can end midway through a UTF-8 code point in the data body.
        let mut prefix = prefix.into_bytes();
        prefix.extend_from_slice(&[b'"', 0xe2, 0x82]);
        let dropped = adapter.receive(json!({"type":"rpc_chunk","chunkId":"large","index":0,"count":2,"byteLength":MAX_PAYLOAD+1,"data":STANDARD.encode(prefix)})).unwrap();
        assert!(
            dropped
                .events
                .iter()
                .any(|event| matches!(event, Event::Subagent { id, .. } if id == "child"))
        );
        adapter.receive(json!({"type":"rpc_chunk","chunkId":"large","index":1,"count":2,"byteLength":MAX_PAYLOAD+1,"data":""})).unwrap();
        let live = adapter.receive(json!({"type":"subagent_event","payload":{"id":"child","event":{"type":"message_end","message":{"role":"assistant","timestamp":123,"content":[{"type":"text","text":"still live"}]}}}})).unwrap();
        assert!(
            live.events
                .iter()
                .any(|event| matches!(event, Event::SubagentItem { id, .. } if id == "child"))
        );
        assert!(adapter.chunks.is_empty());
        assert!(adapter.dropped_chunks.is_empty());
        assert!(adapter.command(Command::Interrupt).is_ok());
    }

    #[test]
    fn child_partial_replaces_overlapping_history_and_preserves_configured_auto() {
        let mut adapter = Omp::default();
        let metadata = adapter.receive(json!({"type":"subagent_progress","payload":{"progress":{"id":"child","status":"running"}}})).unwrap();
        let live = json!({"type":"message_update","assistantMessageEvent":{"partial":{"role":"assistant","timestamp":123,"content":[{"type":"text","text":"hello world"}]},"type":"text_delta","contentIndex":0,"delta":" world"}});
        adapter
            .receive(json!({"type":"subagent_event","payload":{"id":"child","event":live}}))
            .unwrap();
        let hydrated = adapter.receive(json!({"type":"response","id":metadata.writes[0]["id"],"command":"get_subagent_messages","success":true,"data":{"messages":[{"role":"assistant","timestamp":123,"content":[{"type":"text","text":"hello world"}]}]}})).unwrap();
        assert!(
            !hydrated
                .events
                .iter()
                .any(|event| matches!(event, Event::SubagentTextDelta { .. }))
        );
        assert!(hydrated.events.iter().any(|event| matches!(event, Event::SubagentItem { item: Item { content:ItemContent::Text { text, .. }, .. }, .. } if text == "hello world")));
        let thinking = adapter
            .receive(
                json!({"type":"thinking_level_changed","thinkingLevel":"low","configured":"auto"}),
            )
            .unwrap();
        assert!(thinking.events.iter().any(
            |event| matches!(event, Event::Models { reasoning:Some(level), .. } if level == "auto")
        ));
    }

    // Reduced from a real OMP 18.2.9 rpc-ui Muse capture. This build ACKs
    // ordinary prompts and emits agent_end, but never emits prompt_result.
    fn captured_legacy_run(
        acknowledge: bool,
        stop_reason: &str,
    ) -> (Omp, crate::model::RunId, Value) {
        let mut adapter = Omp {
            ready: true,
            started: true,
            session_id: Some("01a131b1-7d42-7126-9a3c-e19d00316562".into()),
            ..Omp::default()
        };
        adapter.usage.cost_microusd = Some(0);
        let run = crate::model::RunId::new();
        let prompt = adapter
            .command_for_run(
                run,
                Command::Prompt {
                    text: "Reply exactly: smoke test complete. Do not use tools.".into(),
                    attachments: vec![],
                    mode: PromptMode::Send,
                },
            )
            .unwrap();
        if acknowledge {
            adapter
                .receive(json!({
                    "id":prompt.writes[0]["id"],"type":"response","command":"prompt","success":true
                }))
                .unwrap();
        }
        adapter.receive(json!({"type":"agent_start"})).unwrap();
        let message = json!({
            "role":"assistant","timestamp":1791835092020u64,
            "content":[{"type":"text","text":"smoke test complete"}],
            "usage":{"input":22426,"output":25,"cost":{"total":0.0022476}},
            "stopReason":stop_reason
        });
        let step = adapter
            .receive(json!({"type":"message_end","message":message}))
            .unwrap();
        assert!(step.events.iter().any(|event| matches!(event,
            Event::Item(Item { content: ItemContent::Text { text, thinking:false, .. }, .. })
                if text == "smoke test complete")));
        assert!(step.events.iter().any(|event| matches!(event,
            Event::Execution { run:actual, event:crate::agent_execution::ProviderEvent::Scope }
                if *actual == run)));
        (adapter, run, message)
    }

    #[test]
    fn captured_legacy_terminal_turn_finishes_after_its_idle_probe_without_prompt_result() {
        use crate::{agent_execution::ProviderEvent, notifications::Kind};
        for (stop_reason, outcome) in [
            ("stop", Kind::Succeeded),
            ("aborted", Kind::Cancelled),
            ("error", Kind::Failed),
        ] {
            let (mut adapter, run, message) = captured_legacy_run(true, stop_reason);
            let end = adapter
                .receive(json!({"type":"agent_end","isTerminal":true,"messages":[message]}))
                .unwrap();
            assert!(end.events.iter().any(|event| matches!(event,
                Event::Execution { run: actual, event:ProviderEvent::Settled } if *actual == run)));
            assert!(end.events.iter().any(|event| matches!(event,
                Event::Usage(usage) if usage.input_tokens == 22426 && usage.output_tokens == 25)));
            assert!(!end.events.iter().any(|event| matches!(
                event,
                Event::Execution {
                    event: ProviderEvent::Finished { .. },
                    ..
                }
            )));
            let probe = end
                .writes
                .iter()
                .find(|frame| frame["type"] == "get_state")
                .unwrap();
            let stale = adapter
                .receive(
                    json!({"type":"response","id":"old","command":"get_state","success":true,
                "data":{"sessionId":adapter.session_id,"isStreaming":false,"isCompacting":false}}),
                )
                .unwrap();
            assert!(stale.events.is_empty());
            let finished = adapter
                .receive(
                    json!({"type":"response","id":probe["id"],"command":"get_state","success":true,
                "data":{"sessionId":adapter.session_id,"isStreaming":false,"isCompacting":false}}),
                )
                .unwrap();
            assert!(finished.events.iter().any(|event| matches!(event,
                Event::Execution { run:actual, event:ProviderEvent::Finished { outcome:actual_outcome } }
                    if *actual == run && *actual_outcome == outcome)));
            assert!(adapter.execution.is_none());
        }
    }

    #[test]
    fn correlated_modern_result_wins_over_pending_legacy_probe() {
        use crate::{agent_execution::ProviderEvent, notifications::Kind};
        for (status, outcome) in [("aborted", Kind::Cancelled), ("error", Kind::Failed)] {
            let (mut adapter, run, message) = captured_legacy_run(true, "stop");
            let request = adapter.execution.as_ref().unwrap().request.clone();
            let end = adapter
                .receive(json!({"type":"agent_end","isTerminal":true,"messages":[message]}))
                .unwrap();
            let probe = end
                .writes
                .iter()
                .find(|frame| frame["type"] == "get_state")
                .unwrap();
            let modern = adapter
                .receive(json!({"type":"prompt_result","id":request,"status":status}))
                .unwrap();
            assert!(modern.events.iter().any(|event| matches!(event,
                Event::Execution { run: actual, event:ProviderEvent::Finished { outcome: actual_outcome } }
                    if *actual == run && *actual_outcome == outcome)));
            let idle = adapter
                .receive(
                    json!({"type":"response","id":probe["id"],"command":"get_state","success":true,
                "data":{"sessionId":adapter.session_id,"isStreaming":false,"isCompacting":false}}),
                )
                .unwrap();
            assert!(!idle.events.iter().any(|event| matches!(
                event,
                Event::Execution {
                    event: ProviderEvent::Finished { .. },
                    ..
                }
            )));
        }
    }

    #[test]
    fn terminal_run_usage_adds_current_run_once_to_prior_totals() {
        let (mut adapter, _, message) = captured_legacy_run(true, "stop");
        let before = chat::Usage {
            input_tokens: 100,
            output_tokens: 200,
            cost_microusd: Some(300),
            ..chat::Usage::default()
        };
        adapter.execution.as_mut().unwrap().usage_before = before.clone();
        adapter.usage = before;
        let second =
            json!({"role":"assistant","usage":{"input":50,"output":10,"cost":{"total":0.0001}}});
        let end = adapter
            .receive(json!({"type":"agent_end","isTerminal":true,"messages":[message,second]}))
            .unwrap();
        assert!(end.events.iter().any(|event| matches!(event,
            Event::Usage(usage) if usage.input_tokens == 22576 && usage.output_tokens == 235 && usage.cost_microusd == Some(2647))));
        adapter.receive(json!({"type":"agent_start"})).unwrap();
        let later = adapter
            .receive(json!({"type":"agent_end","isTerminal":true,"messages":[second]}))
            .unwrap();
        assert!(
            !later
                .events
                .iter()
                .any(|event| matches!(event, Event::Usage(_)))
        );
        assert_eq!(adapter.usage.input_tokens, 22576);
        assert_eq!(adapter.usage.output_tokens, 235);
        assert_eq!(adapter.usage.cost_microusd, Some(2647));
    }

    #[test]
    fn legacy_terminal_probe_requires_acknowledged_start_and_known_outcome() {
        use crate::agent_execution::ProviderEvent;
        for (acknowledge, stop_reason) in [(false, "stop"), (true, "toolUse"), (true, "")] {
            let (mut adapter, _, message) = captured_legacy_run(acknowledge, stop_reason);
            if !acknowledge {
                // Even a later successful ACK cannot associate a turn that
                // began before this request was accepted.
                let request = adapter.execution.as_ref().unwrap().request.clone();
                adapter
                    .receive(
                        json!({"type":"response","id":request,"command":"prompt","success":true}),
                    )
                    .unwrap();
            }
            let end = adapter
                .receive(json!({"type":"agent_end","isTerminal":true,"messages":[message]}))
                .unwrap();
            let probe = end
                .writes
                .iter()
                .find(|frame| frame["type"] == "get_state")
                .unwrap();
            let state = adapter
                .receive(
                    json!({"type":"response","id":probe["id"],"command":"get_state","success":true,
                "data":{"sessionId":adapter.session_id,"isStreaming":false,"isCompacting":false}}),
                )
                .unwrap();
            assert!(!state.events.iter().any(|event| matches!(
                event,
                Event::Execution {
                    event: ProviderEvent::Finished { .. },
                    ..
                }
            )));
            assert!(adapter.execution.is_some());
        }
    }

    #[test]
    fn continuing_or_newer_turns_cannot_complete_a_legacy_terminal_probe() {
        use crate::agent_execution::ProviderEvent;
        for continuing in [json!({"isTerminal":false}), json!({"willContinue":true})] {
            let (mut adapter, _, message) = captured_legacy_run(true, "stop");
            let mut frame = continuing;
            frame["type"] = "agent_end".into();
            frame["messages"] = json!([message]);
            let step = adapter.receive(frame).unwrap();
            assert!(!step.events.iter().any(|event| matches!(
                event,
                Event::Execution {
                    event: ProviderEvent::Settled | ProviderEvent::Finished { .. },
                    ..
                }
            )));
            assert!(adapter.execution.as_ref().unwrap().observing);
            assert!(
                adapter
                    .execution
                    .as_ref()
                    .unwrap()
                    .completion_probe
                    .is_none()
            );
        }
        let (mut adapter, _, message) = captured_legacy_run(true, "stop");
        let end = adapter
            .receive(json!({"type":"agent_end","isTerminal":true,"messages":[message]}))
            .unwrap();
        let probe = end
            .writes
            .iter()
            .find(|frame| frame["type"] == "get_state")
            .unwrap();
        adapter.receive(json!({"type":"agent_start"})).unwrap();
        let stale = adapter
            .receive(
                json!({"type":"response","id":probe["id"],"command":"get_state","success":true,
            "data":{"sessionId":adapter.session_id,"isStreaming":false,"isCompacting":false}}),
            )
            .unwrap();
        assert!(!stale.events.iter().any(|event| matches!(
            event,
            Event::Execution {
                event: ProviderEvent::Finished { .. },
                ..
            }
        )));
        // The old outcome remains available, but finishing it requires a new
        // idle reply obtained after the newer turn's terminal boundary.
        let later = adapter
            .receive(json!({"type":"agent_end","isTerminal":true}))
            .unwrap();
        let fresh = later
            .writes
            .iter()
            .find(|frame| frame["type"] == "get_state")
            .unwrap();
        let finished = adapter
            .receive(
                json!({"type":"response","id":fresh["id"],"command":"get_state","success":true,
            "data":{"sessionId":adapter.session_id,"isStreaming":false,"isCompacting":false}}),
            )
            .unwrap();
        assert!(finished.events.iter().any(|event| matches!(
            event,
            Event::Execution {
                event: ProviderEvent::Finished {
                    outcome: crate::notifications::Kind::Succeeded
                },
                ..
            }
        )));
    }

    #[test]
    fn legacy_probe_requires_explicit_idle_in_the_original_conversation() {
        use crate::agent_execution::ProviderEvent;
        for state in [
            json!({"isStreaming":false}),
            json!({"isCompacting":false}),
            json!({"isStreaming":true,"isCompacting":false}),
            json!({"isStreaming":false,"isCompacting":true}),
            json!({"sessionId":"other","isStreaming":false,"isCompacting":false}),
        ] {
            let (mut adapter, _, message) = captured_legacy_run(true, "stop");
            let end = adapter
                .receive(json!({"type":"agent_end","isTerminal":true,"messages":[message]}))
                .unwrap();
            let probe = end
                .writes
                .iter()
                .find(|frame| frame["type"] == "get_state")
                .unwrap();
            let mut state = state;
            if state.get("sessionId").is_none() {
                state["sessionId"] = adapter.session_id.clone().into();
            }
            let reply = adapter.receive(json!({"type":"response","id":probe["id"],"command":"get_state","success":true,"data":state})).unwrap();
            assert!(!reply.events.iter().any(|event| matches!(
                event,
                Event::Execution {
                    event: ProviderEvent::Finished { .. },
                    ..
                }
            )));
        }
    }

    #[test]
    fn legacy_statusless_consumed_prompt_is_correlated_and_finishes_once() {
        use crate::{agent_execution::ProviderEvent, notifications::Kind};
        let (mut adapter, run, _) = captured_legacy_run(true, "stop");
        let request = adapter.execution.as_ref().unwrap().request.clone();
        let unrelated = adapter
            .receive(json!({"type":"prompt_result","id":"old","agentInvoked":false}))
            .unwrap();
        assert!(!unrelated.events.iter().any(|event| matches!(
            event,
            Event::Execution {
                event: ProviderEvent::Finished { .. },
                ..
            }
        )));
        let result = adapter
            .receive(json!({"type":"prompt_result","id":request,"agentInvoked":false}))
            .unwrap();
        assert!(result.events.iter().any(|event| matches!(event,
            Event::Execution { run:actual, event:ProviderEvent::Finished { outcome:Kind::Succeeded } } if *actual == run)));
        let duplicate = adapter
            .receive(json!({"type":"prompt_result","id":request,"agentInvoked":false}))
            .unwrap();
        assert!(!duplicate.events.iter().any(|event| matches!(
            event,
            Event::Execution {
                event: ProviderEvent::Finished { .. },
                ..
            }
        )));
    }

    #[test]
    fn accepted_prompt_finishes_only_from_its_correlated_prompt_result() {
        use crate::agent_execution::ProviderEvent;
        let mut adapter = Omp {
            ready: true,
            started: true,
            ..Omp::default()
        };
        let run = crate::model::RunId::new();
        let prompt = adapter
            .command_for_run(
                run,
                Command::Prompt {
                    text: "owned prompt".into(),
                    attachments: Vec::new(),
                    mode: PromptMode::Send,
                },
            )
            .unwrap();
        let request = prompt.writes[0]["id"].clone();
        let old_end = adapter
            .receive(json!({"type":"agent_end","isTerminal":true}))
            .unwrap();
        assert!(
            !old_end
                .events
                .iter()
                .any(|event| matches!(event, Event::Execution { .. }))
        );
        adapter.receive(json!({"type":"agent_start"})).unwrap();
        let intermediate = adapter
            .receive(json!({"type":"agent_end","isTerminal":false}))
            .unwrap();
        assert!(!intermediate.events.iter().any(|event| matches!(
            event,
            Event::Execution {
                event: ProviderEvent::Settled | ProviderEvent::Finished { .. },
                ..
            }
        )));
        let end = adapter
            .receive(json!({"type":"agent_end","isTerminal":true}))
            .unwrap();
        assert!(end.events.iter().any(|event| matches!(event,
            Event::Execution { run: actual, event: ProviderEvent::Settled } if *actual == run)));
        assert!(!end.events.iter().any(|event| matches!(
            event,
            Event::Execution {
                event: ProviderEvent::Finished { .. },
                ..
            } | Event::TurnFinished { .. }
        )));
        let unrelated = adapter
            .receive(
                json!({"type":"prompt_result","id":"old","status":"completed","agentInvoked":true}),
            )
            .unwrap();
        assert!(
            !unrelated
                .events
                .iter()
                .any(|event| matches!(event, Event::Execution { .. }))
        );
        let result = adapter
            .receive(
                json!({"type":"prompt_result","id":request,"status":"aborted","agentInvoked":true}),
            )
            .unwrap();
        assert!(result.events.iter().any(|event| matches!(event,
            Event::Execution { run: actual, event: ProviderEvent::Finished { outcome: crate::notifications::Kind::Cancelled } } if *actual == run)));
        assert!(result.events.iter().any(|event| matches!(
            event,
            Event::TurnFinished {
                outcome: crate::notifications::Kind::Cancelled,
                ..
            }
        )));
        assert!(adapter.execution.is_none());
        let next = crate::model::RunId::new();
        adapter
            .command_for_run(
                next,
                Command::Prompt {
                    text: "next prompt".into(),
                    attachments: Vec::new(),
                    mode: PromptMode::Send,
                },
            )
            .unwrap();
        let late = adapter
            .receive(json!({"type":"agent_end","isTerminal":true}))
            .unwrap();
        assert!(
            !late
                .events
                .iter()
                .any(|event| matches!(event, Event::Execution { .. } | Event::TurnFinished { .. }))
        );
        assert_eq!(adapter.execution.as_ref().unwrap().run, next);
        assert!(!adapter.execution.as_ref().unwrap().settled);
    }

    #[test]
    fn attention_waits_for_agent_end_and_ignores_compaction_and_tools() {
        use crate::notifications::Kind;
        for (reason, expected) in [
            ("stop", Some(Kind::Succeeded)),
            ("error", Some(Kind::Failed)),
            ("aborted", Some(Kind::Cancelled)),
            ("toolUse", None),
        ] {
            let mut adapter = Omp::default();
            adapter.receive(json!({"type":"agent_start"})).unwrap();
            let message = adapter.receive(json!({"type":"message_end","message":{"role":"assistant","stopReason":reason,"content":[]}})).unwrap();
            assert!(
                !message
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::TurnFinished { .. }))
            );
            let compact = adapter
                .receive(json!({"type":"auto_compaction_end"}))
                .unwrap();
            assert!(
                !compact
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::TurnFinished { .. }))
            );
            let ended = adapter.receive(json!({"type":"agent_end"})).unwrap();
            assert_eq!(
                ended.events.iter().find_map(|event| match event {
                    Event::TurnFinished { outcome, .. } => Some(*outcome),
                    _ => None,
                }),
                expected
            );
            let duplicate = adapter.receive(json!({"type":"agent_end"})).unwrap();
            assert!(
                !duplicate
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::TurnFinished { .. }))
            );
        }
    }

    #[test]
    fn intermediate_agent_end_does_not_announce_or_settle_the_run() {
        let mut adapter = Omp::default();
        adapter.receive(json!({"type":"agent_start"})).unwrap();
        adapter.receive(json!({"type":"message_end","message":{"role":"assistant","stopReason":"stop","content":[]}})).unwrap();
        let intermediate = adapter
            .receive(json!({"type":"agent_end","isTerminal":false}))
            .unwrap();
        assert!(
            !intermediate
                .events
                .iter()
                .any(|event| matches!(event, Event::TurnFinished { .. } | Event::TurnSettled))
        );
        assert!(adapter.busy);
        let terminal = adapter
            .receive(json!({"type":"agent_end","isTerminal":true}))
            .unwrap();
        assert!(terminal.events.iter().any(|event| matches!(
            event,
            Event::TurnFinished {
                outcome: crate::notifications::Kind::Succeeded,
                ..
            }
        )));
        assert!(
            terminal
                .events
                .iter()
                .any(|event| matches!(event, Event::TurnSettled))
        );
    }

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
    fn extension_questions_include_fieldless_confirmation_without_tool_approval() {
        let mut adapter = Omp {
            ready: true,
            ..Default::default()
        };
        for method in ["select", "input", "editor", "confirm"] {
            let requested = adapter.receive(json!({"type":"extension_ui_request","id":method,"method":method,"title":"Question","message":"Continue?","options":["One","Two"]})).unwrap();
            assert!(
                matches!(&requested.events[0], Event::Request(request) if request.kind == crate::chat::RequestKind::Question && request.fields.is_empty() == (method == "confirm"))
            );
            let response = adapter
                .command(Command::Answer {
                    request_id: method.into(),
                    action: "submit".into(),
                    answers: vec![Answer {
                        field: "value".into(),
                        values: vec!["One".into()],
                    }],
                })
                .unwrap();
            let expected = if method == "confirm" {
                json!({"type":"extension_ui_response","id":method,"confirmed":true})
            } else {
                json!({"type":"extension_ui_response","id":method,"value":"One"})
            };
            assert_eq!(response.writes, vec![expected]);
        }
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

    #[test]
    fn state_queue_contents_preserve_duplicates_and_older_providers_use_local_queueing() {
        for contents in [None, Some(json!(["next", "next", ""]))] {
            let mut adapter = Omp {
                ready: true,
                started: true,
                ..Omp::default()
            };
            let request = adapter.request("get_state");
            let mut data = json!({"sessionId":"session", "isStreaming":true});
            if let Some(contents) = &contents {
                data["queuedMessages"] = json!({"followUp":contents});
            }
            let response = adapter.receive(json!({"type":"response", "command":"get_state", "id":request["id"], "success":true,"data":data})).unwrap();
            assert!(response.events.iter().any(|event| matches!(event, Event::Capabilities(caps) if caps.queue == contents.is_some())));
            assert!(response.events.iter().any(|event| matches!(event, Event::Capabilities(caps) if !caps.stop_subagent && !caps.interrupt_stops_background)));
            if contents.is_some() {
                assert!(response.events.iter().any(|event| matches!(event, Event::QueueChanged(messages) if messages.len()==3 && messages[0].text==messages[1].text && messages[2].text.is_empty())));
            }
        }
    }

    #[test]
    fn queue_ack_and_live_user_echo_refresh_authoritative_state() {
        let mut adapter = Omp {
            ready: true,
            started: true,
            ..Omp::default()
        };
        let ack = adapter
            .receive(json!({"type":"response","command":"follow_up","id":"queue-1","success":true}))
            .unwrap();
        assert_eq!(ack.writes[0]["type"], "get_state");
        let echo = adapter.receive(json!({"type":"message_start","message":{"role":"user","timestamp":123,"content":"next"}})).unwrap();
        assert_eq!(echo.writes[0]["type"], "get_state");
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
        assert!(failure.events.contains(&Event::Status {
            busy: false,
            detail: "Ready".into()
        }));
    }

    /// A reply the conversation needs before it takes commands that fails
    /// ends it, since it could never be ready; once ready, the same failure
    /// is only reported.
    #[test]
    fn a_failed_start_ends_the_conversation() {
        // Once started, a conversation loading another session is not ready
        // but is still not ended.
        for started in [false, true] {
            let mut adapter = Omp {
                started,
                state_request: Some("state".into()),
                ..Default::default()
            };
            let failure = adapter
                .receive(json!({"type":"response","id":"state","command":"get_state","success":false,"error":"No session"}))
                .unwrap();
            assert!(
                matches!(&failure.events[0], Event::Error { message, fatal } if message == "No session" && *fatal != started),
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

    /// A shell command's output is shown without the escapes it wrote for a
    /// terminal, while it runs and when it ends; a file that was read is not
    /// a terminal's output and is shown as it is.
    #[test]
    fn shell_output_loses_its_terminal_escapes_and_a_read_file_does_not() {
        let mut adapter = Omp::default();
        let output = |step: &Step| match &step.events[0] {
            Event::Item(Item {
                content: ItemContent::Command { output, .. },
                ..
            }) => output.clone(),
            other => panic!("{other:?}"),
        };
        adapter.receive(json!({"type":"tool_execution_start","toolCallId":"call","toolName":"bash","args":{"command":"cargo check","cwd":"/repo"}})).unwrap();
        let progressed = adapter.receive(json!({"type":"tool_execution_update","toolCallId":"call","toolName":"bash","partialResult":{"content":[{"type":"text","text":"\u{1b}[32m    Checking\u{1b}[0m a\n 1/2\r 2/2"}]}})).unwrap();
        assert_eq!(output(&progressed), "    Checking a\n 2/2");
        let finished = adapter.receive(json!({"type":"tool_execution_end","toolCallId":"call","toolName":"bash","result":{"content":[{"type":"text","text":"\u{1b}[1;31merror\u{1b}[0m: failed\n"}],"details":{"exitCode":101}}})).unwrap();
        assert_eq!(output(&finished), "error: failed\n");
        adapter.receive(json!({"type":"tool_execution_start","toolCallId":"read","toolName":"read","args":{"path":"palette.txt"}})).unwrap();
        let read = adapter.receive(json!({"type":"tool_execution_end","toolCallId":"read","toolName":"read","result":{"content":[{"type":"text","text":"red = \u{1b}[31m\n"}]}})).unwrap();
        assert_eq!(output(&read), "red = \u{1b}[31m\n");
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
                unset_environment: Default::default(),
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
