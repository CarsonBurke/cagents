//! Codex 0.153.4 app-server JSON-RPC, not TUI output.
//! Wire contracts: `codex app-server generate-json-schema --experimental`,
//! https://github.com/openai/codex/tree/rust-v0.153.4/codex-rs/app-server-protocol
//! Pending requests are replayed to joining connections with the original ID;
//! the first answer wins (app-server/src/outgoing_message.rs). Never auto-answer.
use super::{Adapter, ProcessSpec, Step, TransportSpec};
use crate::chat::{
    Answer, Attachment, Capabilities, Choice, Command, Event, Field, Item, ItemContent, Launch,
    Model, PromptMode, Request, Role, ToolStatus, Usage,
};
use serde_json::{Value, json};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::{collections::BTreeMap, path::PathBuf};

const MAX_PENDING: usize = 128;
const MAX_MODELS: usize = 512;
const MAX_ACTIVE_ITEMS: usize = 128;
const MAX_ITEM_BYTES: usize = 512 * 1024;

fn remote_socket(address: &str) -> Result<Option<PathBuf>, String> {
    let path = address.strip_prefix("unix://").ok_or(
        "Native Codex chat can verify only a local unix:// shared daemon; ws:// and wss:// remote endpoints are unsupported and will not be replaced by a local daemon",
    )?;
    Ok((!path.is_empty()).then(|| PathBuf::from(path)))
}

pub fn create(launch: &Launch) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    if launch
        .session_id
        .as_ref()
        .is_some_and(|id| id.is_empty() || id == "latest")
    {
        return Err("Codex requires an exact thread ID, never a latest-session selector".into());
    }
    let mut spec = ProcessSpec::from(&launch.recipe);
    let mut start = json!({"cwd": spec.working_directory, "experimentalRawEvents": false});
    let mut explicit_socket = None;
    let mut args = launch.recipe.arguments.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "app-server" | "proxy" | "--no-alt-screen" => {}
            "--sock" => {
                explicit_socket = Some(PathBuf::from(
                    args.next().ok_or("Missing Codex --sock path")?,
                ))
            }
            "--remote" => {
                explicit_socket =
                    remote_socket(args.next().ok_or("Missing Codex --remote address")?)?;
            }
            _ if arg.starts_with("--remote=") => {
                explicit_socket = remote_socket(&arg["--remote=".len()..])?;
            }
            "resume" => {
                let id = args
                    .next()
                    .ok_or("Codex resume requires an exact thread ID")?;
                if launch.session_id.as_ref() != Some(id) {
                    return Err("Codex recipe resume ID does not match the selected thread".into());
                }
            }
            "-m" | "--model" | "-s" | "--sandbox" | "-a" | "--ask-for-approval" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("Missing value for Codex {arg}"))?;
                // Attaching must not mutate a running thread's configuration.
                if launch.session_id.is_none() {
                    let key = match arg.as_str() {
                        "-m" | "--model" => "model",
                        "-s" | "--sandbox" => "sandbox",
                        _ => "approvalPolicy",
                    };
                    start[key] = json!(value);
                }
            }
            _ => {
                return Err(format!(
                    "Codex launch option {arg:?} cannot safely be translated to native app-server mode"
                ));
            }
        }
    }
    let home = spec
        .environment
        .get("CODEX_HOME")
        .cloned()
        .or_else(|| std::env::var("CODEX_HOME").ok())
        .map(PathBuf::from)
        .or_else(|| {
            spec.environment
                .get("HOME")
                .cloned()
                .or_else(|| std::env::var("HOME").ok())
                .map(|home| PathBuf::from(home).join(".codex"))
        })
        .ok_or("Cannot locate the existing Codex daemon: CODEX_HOME/HOME are unavailable")?;
    let home = if home.is_absolute() {
        home
    } else {
        spec.working_directory.join(home)
    };
    let expected_home = std::fs::canonicalize(&home).map_err(|e| {
        format!(
            "Cannot verify Codex account directory {}: {e}",
            home.display()
        )
    })?;
    let socket =
        explicit_socket.unwrap_or_else(|| home.join("app-server-control/app-server-control.sock"));
    let socket = if socket.is_absolute() {
        socket
    } else {
        spec.working_directory.join(socket)
    };
    let metadata = std::fs::metadata(&socket).map_err(|error| format!("Cannot join existing Codex daemon at {}: {error}. Start the Codex shared daemon first; no private fallback is launched.", socket.display()))?;
    if !metadata.file_type().is_socket() {
        return Err(format!(
            "Codex endpoint {} is not a Unix socket",
            socket.display()
        ));
    }
    // Linux sun_path is 108 bytes. The symlink resolves to this exact verified
    // socket, not a discovered/newest daemon or a different CODEX_HOME.
    let mut alias = None;
    let connect_socket = if socket.as_os_str().as_encoded_bytes().len() >= 108 {
        let directory =
            PathBuf::from("/tmp").join(format!("cagents-codex-{}", uuid::Uuid::new_v4().simple()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|e| format!("Creating private Codex socket alias: {e}"))?;
        let link = directory.join("s");
        if let Err(error) = std::os::unix::fs::symlink(&socket, &link) {
            let _ = std::fs::remove_dir(&directory);
            return Err(format!("Creating exact Codex socket alias: {error}"));
        }
        let verified = std::fs::metadata(&link)
            .is_ok_and(|m| m.dev() == metadata.dev() && m.ino() == metadata.ino());
        if !verified {
            let _ = std::fs::remove_file(&link);
            let _ = std::fs::remove_dir(&directory);
            return Err(
                "Codex socket changed during alias verification; reconnect explicitly".into(),
            );
        }
        alias = Some(directory);
        link
    } else {
        socket
    };
    spec.arguments.clear();
    spec.transport = TransportSpec::UnixWebSocket {
        socket: connect_socket,
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    Ok((
        spec,
        Box::new(Codex {
            thread: launch.session_id.clone(),
            start,
            alias,
            cwd: launch.recipe.working_directory.clone(),
            expected_home,
            next_id: 0,
            pending: BTreeMap::new(),
            requests: BTreeMap::new(),
            active_items: BTreeMap::new(),
            models: Vec::new(),
            modalities: BTreeMap::new(),
            selected: None,
            reasoning: None,
            selection_dirty: false,
            turn: None,
            busy: false,
            ready: false,
            direct_input: false,
            cursor: None,
            history_loading: false,
            materialized: launch.session_id.is_some(),
        }),
    ))
}

#[derive(Clone, Copy)]
enum Pending {
    Initialize,
    Attach,
    Models,
    History,
    Turn,
    Other,
}
struct PendingRequest {
    frame: Value,
    actions: BTreeMap<String, Value>,
    fields: Vec<Field>,
    kind: RequestKind,
}
enum RequestKind {
    Decision,
    Permissions,
    Questions,
    Elicitation,
}
struct Codex {
    thread: Option<String>,
    start: Value,
    alias: Option<PathBuf>,
    cwd: PathBuf,
    expected_home: PathBuf,
    next_id: u64,
    pending: BTreeMap<String, Pending>,
    requests: BTreeMap<String, PendingRequest>,
    active_items: BTreeMap<String, Item>,
    models: Vec<Model>,
    modalities: BTreeMap<String, Vec<String>>,
    selected: Option<String>,
    reasoning: Option<String>,
    selection_dirty: bool,
    turn: Option<String>,
    busy: bool,
    ready: bool,
    direct_input: bool,
    cursor: Option<String>,
    history_loading: bool,
    materialized: bool,
}
impl Drop for Codex {
    fn drop(&mut self) {
        if let Some(directory) = &self.alias {
            let _ = std::fs::remove_file(directory.join("s"));
            let _ = std::fs::remove_dir(directory);
        }
    }
}
impl Codex {
    fn call(
        &mut self,
        step: &mut Step,
        method: &str,
        params: Value,
        pending: Pending,
    ) -> Result<(), String> {
        if self.pending.len() >= MAX_PENDING {
            return Err("Too many outstanding Codex requests".into());
        }
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or("Codex request ID overflow")?;
        let id = format!("cagents-{}", self.next_id);
        self.pending.insert(json!(id).to_string(), pending);
        step.writes
            .push(json!({"id":id,"method":method,"params":params}));
        Ok(())
    }
    fn capabilities(&self) -> Capabilities {
        let model = self
            .selected
            .as_ref()
            .and_then(|id| self.models.iter().find(|model| &model.id == id));
        Capabilities {
            steer: self.ready && self.direct_input,
            queue: self.ready && self.direct_input,
            attachments: self.direct_input
                && self
                    .selected
                    .as_ref()
                    .and_then(|id| self.modalities.get(id))
                    .is_some_and(|m| m.iter().any(|m| m == "image")),
            compact: self.ready && self.direct_input,
            model_selection: self.direct_input && !self.models.is_empty(),
            reasoning_selection: self.direct_input
                && model.is_some_and(|m| !m.reasoning.is_empty()),
            permission_modes: Vec::new(),
        }
    }
    fn model_events(&self, step: &mut Step) {
        step.events.push(Event::Models {
            models: self.models.clone(),
            selected: self.selected.clone(),
            reasoning: self.reasoning.clone(),
        });
        step.events.push(Event::Capabilities(self.capabilities()));
    }
    fn history(&mut self, step: &mut Step, cursor: Option<String>) -> Result<(), String> {
        if !self.materialized {
            return Ok(());
        }
        if self.history_loading {
            return Err("Codex history is already loading".into());
        }
        self.call(step, "thread/turns/list", json!({"threadId":self.thread,"cursor":cursor,"limit":20,"sortDirection":"desc","itemsView":"full"}), Pending::History)?;
        self.history_loading = true;
        Ok(())
    }
    fn page(&mut self, page: &Value, step: &mut Step) -> Result<(), String> {
        self.cursor = page["nextCursor"].as_str().map(str::to_owned);
        let mut items = Vec::new();
        for turn in array(page, "data")?.iter().rev() {
            let turn_id = string(turn, "id")?;
            let active = turn["status"] == "inProgress";
            if active && self.busy {
                self.turn = Some(turn_id.into());
            }
            if turn["itemsView"]
                .as_str()
                .is_some_and(|view| view != "full")
            {
                return Err("Codex did not return full turn items as requested; history cannot be hydrated safely".into());
            }
            for value in array(turn, "items")? {
                for item in parse_item(value, !active, turn_id)? {
                    if active && self.busy && matches!(item.content, ItemContent::Tool { .. }) {
                        if self.active_items.len() >= MAX_ACTIVE_ITEMS
                            && !self.active_items.contains_key(&item.id)
                        {
                            return Err("Too many live Codex tools".into());
                        }
                        self.active_items
                            .entry(item.id.clone())
                            .or_insert_with(|| item.clone());
                    }
                    items.push(item);
                }
            }
        }
        step.events.push(Event::History {
            items,
            has_earlier: self.cursor.is_some(),
        });
        Ok(())
    }
    fn input(&self, text: String, attachments: Vec<Attachment>) -> Result<Vec<Value>, String> {
        let mut input = Vec::with_capacity(1 + attachments.len());
        if !text.is_empty() {
            input.push(json!({"type":"text","text":text}));
        }
        for attachment in attachments {
            if !self.capabilities().attachments {
                return Err("The selected Codex model does not advertise image input".into());
            }
            let path = if attachment.path.is_absolute() {
                attachment.path
            } else {
                self.cwd.join(attachment.path)
            };
            let extension = path
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
                return Err("Codex native attachments support local images (PNG/JPEG/GIF/WebP), not arbitrary files".into());
            }
            if !path.is_file() {
                return Err(format!("Attachment is not a file: {}", path.display()));
            }
            input.push(json!({"type":"localImage","path":path.to_str().ok_or("Attachment path is not UTF-8")?}));
        }
        if input.is_empty() {
            return Err("Enter a message or attach an image".into());
        }
        Ok(input)
    }
    fn invalidate(&mut self, step: &mut Step) {
        for id in std::mem::take(&mut self.requests).into_keys() {
            step.events.push(Event::RequestResolved { id });
        }
    }
    fn request(&mut self, frame: Value, step: &mut Step) -> Result<(), String> {
        let id = rpc_id(&frame["id"])?;
        let method = string(&frame, "method")?;
        let params = &frame["params"];
        if params["threadId"].as_str() != self.thread.as_deref() {
            // A shared connection may see non-thread-scoped auth requests. Never
            // answer another thread's approval or manufacture authentication.
            step.events.push(Event::Error { message: format!("Codex request {method} is not scoped to this thread and cannot be answered here"), fatal: false });
            return Ok(());
        }
        if self.requests.len() >= MAX_PENDING && !self.requests.contains_key(&id) {
            return Err("Too many pending Codex approval requests".into());
        }
        let mut actions = BTreeMap::new();
        let mut fields = Vec::new();
        let mut description = params["reason"].as_str().unwrap_or("").to_owned();
        let (title, kind) = match method {
            "item/commandExecution/requestApproval" => {
                description = format!(
                    "{}\n{}\nWorking directory: {}",
                    description,
                    params["command"]
                        .as_str()
                        .unwrap_or("(command not supplied)"),
                    params["cwd"].as_str().unwrap_or("(not supplied)")
                );
                for key in ["kind", "additionalPermissions", "networkApprovalContext"] {
                    if !params[key].is_null() {
                        description.push_str(&format!("\n{key}: {}", params[key]));
                    }
                }
                if let Some(decisions) = params["availableDecisions"].as_array() {
                    for (index, decision) in decisions.iter().enumerate() {
                        actions.insert(format!("decision-{index}"), decision.clone());
                    }
                } else {
                    for decision in ["accept", "acceptForSession", "decline", "cancel"] {
                        actions.insert(decision.into(), json!(decision));
                    }
                }
                ("Command approval", RequestKind::Decision)
            }
            "item/fileChange/requestApproval" => {
                description.push_str(&format!(
                    "\nRequested root: {}",
                    params["grantRoot"].as_str().unwrap_or("(not supplied)")
                ));
                for decision in ["accept", "acceptForSession", "decline", "cancel"] {
                    actions.insert(decision.into(), json!(decision));
                }
                ("File change approval", RequestKind::Decision)
            }
            "item/permissions/requestApproval" => {
                description.push_str(&format!(
                    "\nRequested permissions: {}",
                    params["permissions"]
                ));
                actions.insert(
                    "grant-turn".into(),
                    json!({"permissions":params["permissions"],"scope":"turn"}),
                );
                actions.insert(
                    "grant-session".into(),
                    json!({"permissions":params["permissions"],"scope":"session"}),
                );
                actions.insert("deny".into(), json!({"permissions":{},"scope":"turn"}));
                ("Additional permissions", RequestKind::Permissions)
            }
            "item/tool/requestUserInput" => {
                description = if params["isBlocking"]
                    .as_bool()
                    .ok_or("Missing Codex question blocking status")?
                {
                    "Codex is waiting for your answer."
                } else {
                    "Optional question; Codex can continue while you answer."
                }
                .into();
                for question in array(params, "questions")? {
                    fields.push(Field {
                        id: string(question, "id")?.into(),
                        label: string(question, "header")?.into(),
                        description: string(question, "question")?.into(),
                        choices: question["options"]
                            .as_array()
                            .map(|options| {
                                options
                                    .iter()
                                    .map(|option| Choice {
                                        id: option["label"].as_str().unwrap_or("").into(),
                                        label: option["label"].as_str().unwrap_or("").into(),
                                        description: option["description"]
                                            .as_str()
                                            .unwrap_or("")
                                            .into(),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        multiple: false,
                        secret: question["isSecret"].as_bool().unwrap_or(false),
                        initial: String::new(),
                    });
                }
                actions.insert("submit".into(), Value::Null);
                ("Codex questions", RequestKind::Questions)
            }
            "mcpServer/elicitation/request" => {
                description = params["message"].as_str().unwrap_or("").into();
                actions.insert("decline".into(), json!("decline"));
                actions.insert("cancel".into(), json!("cancel"));
                match params["mode"].as_str() {
                    Some("url") => {
                        step.events.push(Event::OpenUrl { url: string(params, "url")?.into(), instructions: description.clone() });
                        // URL elicitation completion is provider-managed, not an invented form acceptance.
                    }
                    Some("form") => {
                        match elicitation_fields(&params["requestedSchema"]) {
                            Ok(value) => { fields = value; actions.insert("accept".into(), json!("accept")); }
                            Err(error) => description.push_str(&format!("\nCannot safely render this form: {error}. Decline/cancel remain available.")),
                        }
                    }
                    _ => description.push_str("\nThis elicitation mode is not supported; decline/cancel remain available."),
                }
                ("MCP server request", RequestKind::Elicitation)
            }
            _ => {
                // An explicit RPC error is fail-closed, unlike a fabricated
                // successful tool result or a permissive default decision.
                step.writes.push(json!({"id":frame["id"],"error":{"code":-32601,"message":format!("Native cagents does not implement {method}")}}));
                step.events.push(Event::Error {
                    message: format!("Unsupported Codex server request: {method}"),
                    fatal: false,
                });
                return Ok(());
            }
        };
        let choices = actions
            .iter()
            .map(|(id, value)| Choice {
                id: id.clone(),
                label: decision_label(id, value),
                description: if value.is_object() {
                    value.to_string()
                } else {
                    String::new()
                },
            })
            .collect();
        step.events.push(Event::Request(Request {
            id: id.clone(),
            title: title.into(),
            description,
            fields: fields.clone(),
            actions: choices,
        }));
        self.requests.insert(
            id,
            PendingRequest {
                frame,
                actions,
                fields,
                kind,
            },
        );
        Ok(())
    }
    fn answer(
        &mut self,
        id: String,
        action: String,
        answers: Vec<Answer>,
        step: &mut Step,
    ) -> Result<(), String> {
        let request = self
            .requests
            .get(&id)
            .ok_or("This Codex request was already resolved or is no longer available")?;
        let choice = request
            .actions
            .get(&action)
            .ok_or("This decision is not offered by the Codex request")?;
        let result = match request.kind {
            RequestKind::Decision => json!({"decision":choice}),
            RequestKind::Permissions => choice.clone(),
            RequestKind::Questions => {
                let questions = array(&request.frame["params"], "questions")?;
                let mut response = serde_json::Map::new();
                for question in questions {
                    let field = string(question, "id")?;
                    let answer = unique_answer(&answers, field)?;
                    if answer.values.len() != 1 || answer.values[0].trim().is_empty() {
                        return Err(format!("Provide one answer for {field}"));
                    }
                    let options = question["options"].as_array();
                    if !question["isOther"].as_bool().unwrap_or(false)
                        && options.is_some_and(|options| {
                            !options.is_empty()
                                && !options
                                    .iter()
                                    .any(|o| o["label"].as_str() == Some(&answer.values[0]))
                        })
                    {
                        return Err(format!("Choose an offered answer for {field}"));
                    }
                    response.insert(field.into(), json!({"answers":answer.values}));
                }
                if answers.len() != questions.len() {
                    return Err("Answers contain unknown or duplicate question IDs".into());
                }
                json!({"answers":response})
            }
            RequestKind::Elicitation => {
                if action == "accept" {
                    let schema = &request.frame["params"]["requestedSchema"];
                    let content = elicitation_answers(schema, &request.fields, &answers)?;
                    json!({"action":"accept","content":content})
                } else {
                    json!({"action":choice})
                }
            }
        };
        step.writes
            .push(json!({"id":request.frame["id"],"result":result}));
        self.requests.remove(&id);
        step.events.push(Event::RequestResolved { id });
        Ok(())
    }
}

impl Adapter for Codex {
    fn initialize(&mut self) -> Result<Step, String> {
        let mut step = Step::default();
        self.call(&mut step, "initialize", json!({"clientInfo":{"name":"cagents","title":"cagents native chat","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}), Pending::Initialize)?;
        Ok(step)
    }
    fn command(&mut self, command: Command) -> Result<Step, String> {
        let mut step = Step::default();
        if !self.ready {
            return Err("Codex native connection is not ready".into());
        }
        match command {
            Command::Prompt { text, attachments, mode } => {
                if !self.direct_input { return Err("This Codex thread does not advertise direct input support".into()); }
                let input = self.input(text, attachments)?;
                match mode {
                    PromptMode::Send => {
                        if self.busy || self.pending.values().any(|p| matches!(p, Pending::Turn)) { return Err("A Codex turn is active; explicitly steer or queue instead".into()); }
                        let mut params = json!({"threadId":self.thread,"input":input});
                        if self.selection_dirty { params["model"] = json!(self.selected); params["effort"] = json!(self.reasoning); }
                        self.call(&mut step, "turn/start", params, Pending::Turn)?;
                    }
                    PromptMode::Steer => {
                        let turn = self.turn.as_ref().ok_or("There is no known active Codex turn to steer")?;
                        self.call(&mut step, "turn/steer", json!({"threadId":self.thread,"expectedTurnId":turn,"input":input}), Pending::Other)?;
                    }
                    PromptMode::Queue => {
                        self.call(&mut step, "thread/queue/add", json!({"threadId":self.thread,"clientUserMessageId":uuid::Uuid::new_v4().to_string(),"input":input}), Pending::Other)?;
                        step.events.push(Event::Status { busy: self.busy, detail: "Submitting to Codex's native queue".into() });
                    }
                }
            }
            Command::Interrupt => {
                let turn = self.turn.as_ref().ok_or("There is no known active Codex turn to interrupt")?;
                self.call(&mut step, "turn/interrupt", json!({"threadId":self.thread,"turnId":turn}), Pending::Other)?;
            }
            Command::Answer { request_id, action, answers } => self.answer(request_id, action, answers, &mut step)?,
            Command::SetModel { model, reasoning } => {
                if !self.direct_input { return Err("This Codex thread does not accept direct input".into()); }
                let selected = self.models.iter().find(|m| m.id == model).ok_or("Choose a model from the Codex model catalog")?;
                if reasoning.as_ref().is_some_and(|r| !selected.reasoning.contains(r)) { return Err("The model does not advertise this reasoning effort".into()); }
                self.selected = Some(model); self.reasoning = reasoning; self.selection_dirty = true;
                self.model_events(&mut step);
                step.events.push(Event::Status { busy: self.busy, detail: "Model/reasoning selection will apply to your next new turn, not the running turn".into() });
            }
            Command::SetPermissionMode { .. } => return Err("Codex permission profiles cannot safely be represented by this control; existing permissions are preserved and individual approvals remain explicit".into()),
            Command::Compact => {
                if !self.capabilities().compact || self.busy { return Err("Codex compaction requires an idle, writable thread".into()); }
                self.call(&mut step, "thread/compact/start", json!({"threadId":self.thread}), Pending::Other)?;
            }
            Command::Refresh => {
                if self.history_loading { return Err("Codex history is already loading".into()); }
                if self.pending.len() > MAX_PENDING - 2 { return Err("Too many outstanding Codex requests".into()); }
                self.call(&mut step, "thread/read", json!({"threadId":self.thread,"includeTurns":false}), Pending::Other)?;
                self.history(&mut step, None)?;
            }
            Command::LoadEarlier => {
                let cursor = self.cursor.clone().ok_or("No earlier Codex history is available")?;
                self.history(&mut step, Some(cursor))?;
            }
            Command::Slash { .. } => return Err("Codex app-server has no TUI slash-command dispatcher; use the native model, compact, interrupt and prompt controls".into()),
        }
        Ok(step)
    }
    fn receive(&mut self, frame: Value) -> Result<Step, String> {
        let mut step = Step::default();
        if !frame.is_object() {
            return Err("Codex sent a non-object JSON-RPC frame".into());
        }
        if frame.get("method").is_some() && frame.get("id").is_some() {
            self.request(frame, &mut step)?;
            return Ok(step);
        }
        if frame.get("id").is_some() {
            let id = rpc_id(&frame["id"])?;
            let pending = self
                .pending
                .remove(&id)
                .ok_or("Codex response has an unknown request ID")?;
            if matches!(pending, Pending::History) {
                self.history_loading = false;
            }
            if let Some(error) = frame.get("error") {
                let fatal = matches!(pending, Pending::Initialize | Pending::Attach);
                step.events.push(Event::Error {
                    message: format!(
                        "Codex RPC error: {}",
                        error["message"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| error.to_string())
                    ),
                    fatal,
                });
                return Ok(step);
            }
            let result = frame
                .get("result")
                .ok_or("Codex RPC response is missing result/error")?;
            match pending {
                Pending::Initialize => {
                    if !result["userAgent"].is_string() {
                        return Err("Unsupported Codex initialize response".into());
                    }
                    let server_home =
                        std::fs::canonicalize(string(result, "codexHome")?).map_err(|e| {
                            format!("Cannot verify connected Codex account directory: {e}")
                        })?;
                    if server_home != self.expected_home {
                        return Err("The selected Codex daemon belongs to a different CODEX_HOME; refusing to resume there".into());
                    }
                    step.writes
                        .push(json!({"method":"initialized","params":{}}));
                    if self.thread.is_some() {
                        self.call(&mut step, "thread/resume", json!({"threadId":self.thread,"excludeTurns":true,"initialTurnsPage":{"limit":20,"sortDirection":"desc","itemsView":"full"}}), Pending::Attach)?;
                    } else {
                        self.call(
                            &mut step,
                            "thread/start",
                            self.start.clone(),
                            Pending::Attach,
                        )?;
                    }
                    self.call(
                        &mut step,
                        "model/list",
                        json!({"limit":100}),
                        Pending::Models,
                    )?;
                }
                Pending::Attach => {
                    let thread = &result["thread"];
                    let id = string(thread, "id")?;
                    if self.thread.as_ref().is_some_and(|expected| expected != id) {
                        return Err(
                            "Codex resumed a different thread; refusing conversation takeover"
                                .into(),
                        );
                    }
                    self.thread = Some(id.into());
                    self.ready = true;
                    self.direct_input = thread["canAcceptDirectInput"].as_bool() == Some(true);
                    self.selected = result["model"].as_str().map(str::to_owned);
                    self.reasoning = result["reasoningEffort"].as_str().map(str::to_owned);
                    self.busy = thread["status"]["type"] == "active";
                    step.events.push(Event::Session {
                        id: id.into(),
                        title: thread["name"].as_str().map(str::to_owned),
                    });
                    step.events.push(Event::Status {
                        busy: self.busy,
                        detail: "Connected to Codex shared daemon".into(),
                    });
                    step.events.push(Event::Commands(Vec::new()));
                    self.model_events(&mut step);
                    if result["initialTurnsPage"].is_object() {
                        self.page(&result["initialTurnsPage"], &mut step)?;
                    } else {
                        self.history(&mut step, None)?;
                    }
                }
                Pending::Models => {
                    for value in array(result, "data")? {
                        let id = string(value, "model")?.to_owned();
                        let reasoning = array(value, "supportedReasoningEfforts")?
                            .iter()
                            .map(|e| string(e, "reasoningEffort").map(str::to_owned))
                            .collect::<Result<Vec<_>, _>>()?;
                        let model = Model {
                            id: id.clone(),
                            label: string(value, "displayName")?.into(),
                            reasoning,
                        };
                        if let Some(existing) = self.models.iter_mut().find(|m| m.id == id) {
                            *existing = model;
                        } else {
                            self.models.push(model);
                        }
                        if self.models.len() > MAX_MODELS {
                            return Err("Codex model catalog exceeds native client bound".into());
                        }
                        self.modalities.insert(
                            id,
                            value["inputModalities"]
                                .as_array()
                                .map(|m| {
                                    m.iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_owned)
                                        .collect()
                                })
                                .unwrap_or_else(|| vec!["text".into(), "image".into()]),
                        );
                    }
                    if let Some(cursor) = result["nextCursor"].as_str() {
                        self.call(
                            &mut step,
                            "model/list",
                            json!({"limit":100,"cursor":cursor}),
                            Pending::Models,
                        )?;
                    }
                    self.model_events(&mut step);
                }
                Pending::History => self.page(result, &mut step)?,
                Pending::Turn => {
                    self.materialized = true;
                    self.turn = Some(string(&result["turn"], "id")?.into());
                    self.busy = true;
                    // Preserve selection until an explicit user change. No settings
                    // update RPC is sent while another client's turn is running.
                    step.events.push(Event::Status {
                        busy: true,
                        detail: "Codex turn started".into(),
                    });
                }
                Pending::Other => {
                    if result["thread"].is_object() {
                        self.thread_state(&result["thread"], &mut step)?;
                    }
                }
            }
            return Ok(step);
        }
        let method = string(&frame, "method")?;
        let params = &frame["params"];
        let thread = params["threadId"]
            .as_str()
            .or_else(|| params["thread"]["id"].as_str());
        if thread.is_some() && thread != self.thread.as_deref() {
            return Ok(step);
        }
        match method {
            "thread/started" => {
                if self.ready {
                    self.thread_state(&params["thread"], &mut step)?;
                }
            }
            "thread/status/changed" => {
                self.busy = params["status"]["type"] == "active";
                if !self.busy {
                    self.turn = None;
                }
                step.events.push(Event::Status {
                    busy: self.busy,
                    detail: params["status"]["type"]
                        .as_str()
                        .unwrap_or("unknown")
                        .into(),
                });
            }
            "thread/name/updated" => step.events.push(Event::Session {
                id: self.thread.clone().ok_or("Missing Codex thread")?,
                title: params["threadName"].as_str().map(str::to_owned),
            }),
            "thread/settings/updated" => {
                if !self.selection_dirty {
                    self.selected = params["threadSettings"]["model"]
                        .as_str()
                        .map(str::to_owned);
                    self.reasoning = params["threadSettings"]["effort"]
                        .as_str()
                        .map(str::to_owned);
                    self.model_events(&mut step);
                }
            }
            "turn/started" | "turn/completed" => {
                self.materialized = true;
                let turn = &params["turn"];
                self.busy = method == "turn/started";
                self.turn = if self.busy {
                    Some(string(turn, "id")?.into())
                } else {
                    None
                };
                for value in array(turn, "items")? {
                    for item in parse_item(value, !self.busy, string(turn, "id")?)? {
                        if self.busy && matches!(item.content, ItemContent::Tool { .. }) {
                            if self.active_items.len() >= MAX_ACTIVE_ITEMS
                                && !self.active_items.contains_key(&item.id)
                            {
                                return Err("Too many live Codex tools".into());
                            }
                            self.active_items.insert(item.id.clone(), item.clone());
                        }
                        step.events.push(Event::Item(item));
                    }
                }
                // Non-blocking questions can outlive their originating turn.
                // Only serverRequest/resolved or thread closure invalidates them.
                if !self.busy {
                    self.active_items.clear();
                }
                step.events.push(Event::Status {
                    busy: self.busy,
                    detail: turn["status"]
                        .as_str()
                        .unwrap_or("Codex turn updated")
                        .into(),
                });
                if turn["error"].is_object() {
                    step.events.push(Event::Error {
                        message: turn["error"]["message"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| turn["error"].to_string()),
                        fatal: false,
                    });
                }
            }
            "item/started" | "item/completed" => {
                let done = method == "item/completed";
                for item in parse_item(&params["item"], done, string(params, "turnId")?)? {
                    if done {
                        self.active_items.remove(&item.id);
                    } else if matches!(item.content, ItemContent::Tool { .. }) {
                        if self.active_items.len() >= MAX_ACTIVE_ITEMS
                            && !self.active_items.contains_key(&item.id)
                        {
                            return Err("Too many live Codex tools".into());
                        }
                        self.active_items.insert(item.id.clone(), item.clone());
                    }
                    step.events.push(Event::Item(item));
                }
            }
            "item/agentMessage/delta"
            | "item/plan/delta"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/textDelta" => {
                let base = item_key(string(params, "turnId")?, string(params, "itemId")?);
                let thinking = method.contains("reasoning");
                let id = if thinking {
                    let (kind, index) = if method.ends_with("summaryTextDelta") {
                        (
                            "summary",
                            params["summaryIndex"]
                                .as_u64()
                                .ok_or("Missing reasoning summary index")?,
                        )
                    } else {
                        (
                            "content",
                            params["contentIndex"]
                                .as_u64()
                                .ok_or("Missing reasoning content index")?,
                        )
                    };
                    format!("{base}/{kind}/{index}")
                } else {
                    base.into()
                };
                step.events.push(Event::TextDelta {
                    id,
                    role: Role::Assistant,
                    thinking,
                    delta: string(params, "delta")?.into(),
                });
            }
            "item/commandExecution/outputDelta"
            | "item/fileChange/outputDelta"
            | "item/mcpToolCall/progress" => {
                let id = item_key(string(params, "turnId")?, string(params, "itemId")?);
                if let Some(item) = self.active_items.get_mut(&id) {
                    if let ItemContent::Tool { output, .. } = &mut item.content {
                        let delta = if method.ends_with("progress") {
                            string(params, "message")?
                        } else {
                            string(params, "delta")?
                        };
                        if output.len().saturating_add(delta.len()) > MAX_ITEM_BYTES {
                            return Err("Codex live tool output exceeds native client bound".into());
                        }
                        output.push_str(delta);
                        step.events.push(Event::Item(item.clone()));
                    }
                } else {
                    return Err(format!(
                        "Codex tool delta arrived without a live item: {id}"
                    ));
                }
            }
            "item/fileChange/patchUpdated" => {
                let id = item_key(string(params, "turnId")?, string(params, "itemId")?);
                if let Some(item) = self.active_items.get_mut(&id) {
                    if let ItemContent::Tool { input, .. } = &mut item.content {
                        *input = params["changes"].to_string();
                        step.events.push(Event::Item(item.clone()));
                    }
                }
            }
            "serverRequest/resolved" => {
                let id = rpc_id(&params["requestId"])?;
                self.requests.remove(&id);
                step.events.push(Event::RequestResolved { id });
            }
            "thread/tokenUsage/updated" => {
                let usage = &params["tokenUsage"];
                step.events.push(Event::Usage(Usage {
                    input_tokens: usage["total"]["inputTokens"].as_u64().unwrap_or(0),
                    output_tokens: usage["total"]["outputTokens"].as_u64().unwrap_or(0),
                    context_tokens: usage["last"]["totalTokens"].as_u64().unwrap_or(0),
                    context_limit: usage["modelContextWindow"].as_u64().unwrap_or(0),
                    cost_microusd: None,
                }));
            }
            "thread/closed" | "thread/deleted" => {
                self.ready = false;
                self.busy = false;
                self.invalidate(&mut step);
                step.events.push(Event::Error {
                    message: format!("Codex {method}; reconnect explicitly"),
                    fatal: true,
                });
            }
            "error" => step.events.push(Event::Error {
                message: params["error"]["message"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| params["error"].to_string()),
                fatal: false,
            }),
            "warning" | "configWarning" | "guardianWarning" | "deprecationNotice" => {
                step.events.push(Event::Error {
                    message: params.to_string(),
                    fatal: false,
                })
            }
            "turn/diff/updated" => step.events.push(Event::Item(Item {
                id: format!("{}/diff", string(params, "turnId")?),
                role: Role::Assistant,
                content: ItemContent::Tool {
                    name: "Turn diff".into(),
                    input: String::new(),
                    output: string(params, "diff")?.into(),
                    status: ToolStatus::Succeeded,
                },
            })),
            "turn/plan/updated" => step.events.push(Event::Item(Item {
                id: format!("{}/plan", string(params, "turnId")?),
                role: Role::Assistant,
                content: ItemContent::Text {
                    text: params["plan"].to_string(),
                    thinking: false,
                },
            })),
            "thread/queue/changed" => step.events.push(Event::Status {
                busy: self.busy,
                detail: "Codex's native queue changed".into(),
            }),
            "item/reasoning/summaryPartAdded" | "thread/compacted" => {}
            // Auxiliary app-server notifications are not transcript data. Unknown
            // item types and requests, in contrast, are visibly rejected above.
            _ => {}
        }
        Ok(step)
    }
}
impl Codex {
    fn thread_state(&mut self, thread: &Value, step: &mut Step) -> Result<(), String> {
        let id = string(thread, "id")?;
        if self.thread.as_deref() != Some(id) {
            return Err("Codex metadata belongs to a different thread".into());
        }
        self.direct_input = thread["canAcceptDirectInput"].as_bool() == Some(true);
        self.busy = thread["status"]["type"] == "active";
        step.events.push(Event::Session {
            id: id.into(),
            title: thread["name"].as_str().map(str::to_owned),
        });
        step.events.push(Event::Status {
            busy: self.busy,
            detail: thread["status"].to_string(),
        });
        step.events.push(Event::Capabilities(self.capabilities()));
        Ok(())
    }
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("Unsupported Codex frame: missing string {key}"))
}
fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    value[key]
        .as_array()
        .ok_or_else(|| format!("Unsupported Codex frame: missing array {key}"))
}
fn rpc_id(value: &Value) -> Result<String, String> {
    match value {
        Value::String(_) => Ok(value.to_string()),
        Value::Number(id) if id.is_i64() || id.is_u64() => Ok(value.to_string()),
        _ => Err("Invalid Codex RPC request ID".into()),
    }
}
fn unique_answer<'a>(answers: &'a [Answer], field: &str) -> Result<&'a Answer, String> {
    let mut matches = answers.iter().filter(|answer| answer.field == field);
    let answer = matches
        .next()
        .ok_or_else(|| format!("Missing answer for {field}"))?;
    if matches.next().is_some() {
        return Err(format!("Duplicate answer for {field}"));
    }
    Ok(answer)
}
fn decision_label(id: &str, value: &Value) -> String {
    match value.as_str().unwrap_or(id) {
        "accept" => "Allow once",
        "acceptForSession" => "Allow for session",
        "decline" | "deny" => "Deny",
        "cancel" => "Deny and cancel",
        "grant-turn" => "Grant for this turn",
        "grant-session" => "Grant for session",
        "submit" => "Submit answers",
        _ => return value.to_string(),
    }
    .into()
}
fn item_key(turn: &str, item: &str) -> String {
    format!("{turn}/{item}")
}
fn parse_item(value: &Value, done: bool, turn: &str) -> Result<Vec<Item>, String> {
    let scoped_id = item_key(turn, string(value, "id")?);
    let id = scoped_id.as_str();
    let kind = string(value, "type")?;
    let text = |id: String, text: String, thinking: bool, role: Role| Item {
        id,
        role,
        content: ItemContent::Text { text, thinking },
    };
    let status = match value["status"].as_str() {
        _ if value["success"] == false
            || value["error"].is_object()
            || value["exitCode"].as_i64().is_some_and(|code| code != 0) =>
        {
            ToolStatus::Failed
        }
        Some("failed") => ToolStatus::Failed,
        Some("declined") => ToolStatus::Denied,
        Some("inProgress") => ToolStatus::Running,
        Some("completed") => ToolStatus::Succeeded,
        _ if done => ToolStatus::Succeeded,
        _ => ToolStatus::Running,
    };
    let tool = |name: String, input: String, output: String| {
        vec![Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Tool {
                name,
                input,
                output,
                status,
            },
        }]
    };
    let items = match kind {
        "agentMessage" | "plan" => vec![text(
            id.into(),
            string(value, "text")?.into(),
            false,
            Role::Assistant,
        )],
        "userMessage" => {
            let mut items = Vec::new();
            for (index, content) in array(value, "content")?.iter().enumerate() {
                let part_id = format!("{id}/input/{index}");
                match string(content, "type")? {
                    "text" => items.push(text(
                        part_id,
                        string(content, "text")?.into(),
                        false,
                        Role::User,
                    )),
                    "localImage" | "image" => items.push(Item {
                        id: part_id,
                        role: Role::User,
                        content: ItemContent::Image {
                            path: content["path"]
                                .as_str()
                                .or_else(|| content["url"].as_str())
                                .ok_or("Missing image path")?
                                .into(),
                            description: "User attachment".into(),
                        },
                    }),
                    "mention" | "skill" | "audio" | "localAudio" => {
                        items.push(text(part_id, content.to_string(), false, Role::User))
                    }
                    other => return Err(format!("Unsupported Codex user input item: {other}")),
                }
            }
            items
        }
        "reasoning" => {
            let mut items = Vec::new();
            for field in ["summary", "content"] {
                if let Some(parts) = value[field].as_array() {
                    for (index, part) in parts.iter().enumerate() {
                        items.push(text(
                            format!("{id}/{field}/{index}"),
                            part.as_str().ok_or("Invalid reasoning text")?.into(),
                            true,
                            Role::Assistant,
                        ));
                    }
                }
            }
            items
        }
        "commandExecution" => tool(
            "Command".into(),
            format!(
                "{}\nWorking directory: {}",
                string(value, "command")?,
                string(value, "cwd")?
            ),
            value["aggregatedOutput"].as_str().unwrap_or("").into(),
        ),
        "fileChange" => tool(
            "File changes".into(),
            value["changes"].to_string(),
            String::new(),
        ),
        "mcpToolCall" => tool(
            format!("{} / {}", string(value, "server")?, string(value, "tool")?),
            value["arguments"].to_string(),
            if !value["error"].is_null() {
                value["error"].to_string()
            } else if !value["result"].is_null() {
                value["result"].to_string()
            } else {
                String::new()
            },
        ),
        "dynamicToolCall" => tool(
            string(value, "tool")?.into(),
            value["arguments"].to_string(),
            value["contentItems"].to_string(),
        ),
        "collabAgentToolCall" => tool(
            string(value, "tool")?.into(),
            value["prompt"].as_str().unwrap_or("").into(),
            value["agentsStates"].to_string(),
        ),
        "webSearch" => tool(
            "Web search".into(),
            string(value, "query")?.into(),
            value["results"].to_string(),
        ),
        "functionCallOutput" => tool(
            string(value, "name")?.into(),
            String::new(),
            value["output"].to_string(),
        ),
        "imageView" => vec![Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Image {
                path: string(value, "path")?.into(),
                description: "Codex image".into(),
            },
        }],
        "imageGeneration" => tool(
            "Image generation".into(),
            value["revisedPrompt"].as_str().unwrap_or("").into(),
            value["savedPath"]
                .as_str()
                .unwrap_or_else(|| value["result"].as_str().unwrap_or(""))
                .into(),
        ),
        "enteredReviewMode" | "exitedReviewMode" => vec![text(
            id.into(),
            string(value, "review")?.into(),
            false,
            Role::System,
        )],
        "contextCompaction" => vec![text(
            id.into(),
            "Context compacted".into(),
            false,
            Role::System,
        )],
        "hookPrompt" => vec![text(
            id.into(),
            value["fragments"].to_string(),
            false,
            Role::System,
        )],
        "sleep" | "subAgentActivity" => tool(kind.into(), String::new(), value.to_string()),
        other => return Err(format!("Unsupported Codex transcript item type: {other}")),
    };
    for item in &items {
        let bytes = match &item.content {
            ItemContent::Text { text, .. } => text.len(),
            ItemContent::Tool {
                name,
                input,
                output,
                ..
            } => name
                .len()
                .saturating_add(input.len())
                .saturating_add(output.len()),
            ItemContent::Image { path, description } => {
                path.len().saturating_add(description.len())
            }
        };
        if bytes > MAX_ITEM_BYTES {
            return Err("Codex item exceeds native client size bound".into());
        }
    }
    Ok(items)
}

// MCP's primitive form schema is deliberately validated before offering Accept.
// Rich/unknown schemas remain cancellable, never flattened into an unsafe form.
fn elicitation_fields(schema: &Value) -> Result<Vec<Field>, String> {
    let properties = schema["properties"]
        .as_object()
        .ok_or("Missing form properties")?;
    let mut fields = Vec::new();
    for (id, property) in properties {
        let kind = string(property, "type")?;
        if !matches!(kind, "string" | "boolean" | "integer" | "number") {
            return Err(format!("Unsupported form field type {kind}"));
        }
        if property.as_object().is_some_and(|p| {
            p.keys().any(|k| {
                !matches!(
                    k.as_str(),
                    "type" | "title" | "description" | "default" | "enum" | "enumNames"
                )
            })
        }) {
            return Err(format!("Unsupported validation constraints for {id}"));
        }
        let choices = if kind == "boolean" {
            vec![
                Choice {
                    id: "true".into(),
                    label: "Yes".into(),
                    description: String::new(),
                },
                Choice {
                    id: "false".into(),
                    label: "No".into(),
                    description: String::new(),
                },
            ]
        } else {
            property["enum"]
                .as_array()
                .map(|options| {
                    options
                        .iter()
                        .enumerate()
                        .map(|(index, option)| Choice {
                            id: option
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| option.to_string()),
                            label: property["enumNames"][index]
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| {
                                    option
                                        .as_str()
                                        .map(str::to_owned)
                                        .unwrap_or_else(|| option.to_string())
                                }),
                            description: String::new(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        fields.push(Field {
            id: id.clone(),
            label: property["title"].as_str().unwrap_or(id).into(),
            description: property["description"].as_str().unwrap_or("").into(),
            choices,
            multiple: false,
            secret: false,
            initial: property["default"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    if property["default"].is_null() {
                        String::new()
                    } else {
                        property["default"].to_string()
                    }
                }),
        });
    }
    Ok(fields)
}
fn elicitation_answers(
    schema: &Value,
    fields: &[Field],
    answers: &[Answer],
) -> Result<Value, String> {
    if answers
        .iter()
        .any(|answer| !fields.iter().any(|field| field.id == answer.field))
    {
        return Err("Unknown form field".into());
    }
    let mut content = serde_json::Map::new();
    for field in fields {
        let required = schema["required"]
            .as_array()
            .is_some_and(|r| r.iter().any(|r| r.as_str() == Some(&field.id)));
        let answer = match unique_answer(answers, &field.id) {
            Ok(answer) => answer,
            Err(_) if !required && !answers.iter().any(|a| a.field == field.id) => continue,
            Err(error) => return Err(error),
        };
        if answer.values.len() != 1 {
            return Err(format!("Expected one value for {}", field.id));
        }
        let raw = &answer.values[0];
        if !required && raw.is_empty() {
            continue;
        }
        let property = &schema["properties"][&field.id];
        let value = match string(property, "type")? {
            "string" => json!(raw),
            "boolean" => json!(raw.parse::<bool>().map_err(|_| "Expected true/false")?),
            "integer" => json!(raw.parse::<i64>().map_err(|_| "Expected integer")?),
            "number" => {
                let number = raw.parse::<f64>().map_err(|_| "Expected number")?;
                if !number.is_finite() {
                    return Err("Expected finite number".into());
                }
                json!(number)
            }
            _ => return Err("Unsupported form field type".into()),
        };
        if property["enum"]
            .as_array()
            .is_some_and(|choices| !choices.contains(&value))
        {
            return Err(format!("Choose an offered value for {}", field.id));
        }
        content.insert(field.id.clone(), value);
    }
    Ok(Value::Object(content))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> Codex {
        Codex {
            thread: Some("thread-exact".into()),
            start: json!({}),
            alias: None,
            cwd: PathBuf::from("/tmp"),
            expected_home: std::fs::canonicalize("/tmp").unwrap(),
            next_id: 0,
            pending: BTreeMap::new(),
            requests: BTreeMap::new(),
            active_items: BTreeMap::new(),
            models: vec![Model {
                id: "model".into(),
                label: "Model".into(),
                reasoning: vec!["future-effort".into()],
            }],
            modalities: BTreeMap::new(),
            selected: Some("model".into()),
            reasoning: None,
            selection_dirty: false,
            turn: None,
            busy: false,
            ready: true,
            direct_input: true,
            cursor: None,
            history_loading: false,
            materialized: true,
        }
    }

    #[test]
    fn joins_exact_thread_without_settings_or_history_overrides() {
        let mut adapter = adapter();
        let initialize = adapter.initialize().unwrap();
        let step = adapter.receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":"/tmp"}})).unwrap();
        let resume = step
            .writes
            .iter()
            .find(|frame| frame["method"] == "thread/resume")
            .unwrap();
        assert_eq!(
            resume["params"],
            json!({"threadId":"thread-exact","excludeTurns":true,"initialTurnsPage":{"limit":20,"sortDirection":"desc","itemsView":"full"}})
        );
        let wrong = adapter
            .receive(json!({"id":resume["id"],"result":{"thread":{"id":"tree-id-not-thread"}}}));
        assert!(wrong.is_err());
    }

    #[test]
    fn new_thread_defers_history_until_its_first_turn_materializes() {
        let mut adapter = adapter();
        adapter.thread = None;
        adapter.materialized = false;
        let initialize = adapter.initialize().unwrap();
        let setup = adapter
            .receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":"/tmp"}}))
            .unwrap();
        let start = setup
            .writes
            .iter()
            .find(|frame| frame["method"] == "thread/start")
            .unwrap();
        let attached = adapter.receive(json!({"id":start["id"],"result":{"thread":{"id":"fresh","canAcceptDirectInput":true,"status":{"type":"idle"}},"model":"gpt-test"}})).unwrap();
        assert!(
            attached
                .events
                .iter()
                .any(|event| matches!(event, Event::Session { id, .. } if id == "fresh"))
        );
        assert!(
            !attached
                .writes
                .iter()
                .any(|frame| frame["method"] == "thread/turns/list")
        );
        adapter.receive(json!({"method":"turn/started","params":{"threadId":"fresh","turn":{"id":"turn","status":"inProgress","items":[]}}})).unwrap();
        let refreshed = adapter.command(Command::Refresh).unwrap();
        assert!(
            refreshed
                .writes
                .iter()
                .any(|frame| frame["method"] == "thread/turns/list")
        );
    }

    #[test]
    fn request_ids_and_resolution_prevent_wrong_or_stale_approval() {
        let mut adapter = adapter();
        for id in [json!(7), json!("7")] {
            let step = adapter.receive(json!({"id":id,"method":"item/commandExecution/requestApproval","params":{"threadId":"thread-exact","turnId":"turn","itemId":"tool","availableDecisions":["accept","decline"],"command":"echo requested"}})).unwrap();
            assert!(step.writes.is_empty());
        }
        assert_ne!(rpc_id(&json!(7)).unwrap(), rpc_id(&json!("7")).unwrap());
        adapter.receive(json!({"method":"serverRequest/resolved","params":{"threadId":"thread-exact","requestId":7}})).unwrap();
        assert!(
            adapter
                .command(Command::Answer {
                    request_id: rpc_id(&json!(7)).unwrap(),
                    action: "decision-0".into(),
                    answers: vec![]
                })
                .is_err()
        );
        let answered = adapter
            .command(Command::Answer {
                request_id: rpc_id(&json!("7")).unwrap(),
                action: "decision-1".into(),
                answers: vec![],
            })
            .unwrap();
        assert_eq!(
            answered.writes,
            json!([{"id":"7","result":{"decision":"decline"}}])
                .as_array()
                .unwrap()
                .clone()
        );
    }

    #[test]
    fn resumed_tool_and_final_history_use_same_turn_scoped_identity() {
        let mut adapter = adapter();
        adapter.busy = true;
        let item = json!({"id":"0","type":"commandExecution","command":"echo hello","cwd":"/tmp","status":"inProgress","aggregatedOutput":"hel"});
        let mut page = Step::default();
        adapter.page(&json!({"data":[{"id":"turn-a","status":"inProgress","items":[item]}],"nextCursor":"older"}),&mut page).unwrap();
        let delta = adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"turn-a","itemId":"0","delta":"lo"}})).unwrap();
        assert!(
            matches!(&delta.events[0],Event::Item(Item { id,content:ItemContent::Tool { output,.. },.. }) if id == "turn-a/0" && output == "hello")
        );
        let completed = json!({"id":"0","type":"agentMessage","text":"answer"});
        let one = parse_item(&completed, true, "turn-a").unwrap();
        let two = parse_item(&completed, true, "turn-b").unwrap();
        assert_ne!(one[0].id, two[0].id);
        let live = adapter.receive(json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact","turnId":"turn-a","itemId":"0","delta":"answer"}})).unwrap();
        assert!(matches!(&live.events[0],Event::TextDelta { id,.. } if id == &one[0].id));
    }

    #[test]
    fn changing_model_never_sends_control_to_a_running_turn() {
        let mut adapter = adapter();
        adapter.busy = true;
        adapter.turn = Some("active".into());
        let change = adapter
            .command(Command::SetModel {
                model: "model".into(),
                reasoning: Some("future-effort".into()),
            })
            .unwrap();
        assert!(change.writes.is_empty());
        let steer = adapter
            .command(Command::Prompt {
                text: "follow up".into(),
                attachments: vec![],
                mode: PromptMode::Steer,
            })
            .unwrap();
        assert_eq!(
            steer.writes[0]["params"],
            json!({"threadId":"thread-exact","expectedTurnId":"active","input":[{"type":"text","text":"follow up"}]})
        );
        assert!(
            adapter
                .command(Command::Prompt {
                    text: "new".into(),
                    attachments: vec![],
                    mode: PromptMode::Send
                })
                .is_err()
        );
    }

    #[test]
    fn optional_questions_survive_turn_completion_until_resolved() {
        let mut adapter = adapter();
        adapter.receive(json!({"id":"q","method":"item/tool/requestUserInput","params":{"threadId":"thread-exact","turnId":"turn","itemId":"question","isBlocking":false,"questions":[{"id":"scope","header":"Scope","question":"Which?","isOther":false,"options":[{"label":"One","description":"One file"}]}]}})).unwrap();
        adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"turn","status":"completed","items":[]}}})).unwrap();
        let invalid = adapter.command(Command::Answer {
            request_id: rpc_id(&json!("q")).unwrap(),
            action: "submit".into(),
            answers: vec![Answer {
                field: "scope".into(),
                values: vec!["All".into()],
            }],
        });
        assert!(invalid.is_err());
        let answer = adapter
            .command(Command::Answer {
                request_id: rpc_id(&json!("q")).unwrap(),
                action: "submit".into(),
                answers: vec![Answer {
                    field: "scope".into(),
                    values: vec!["One".into()],
                }],
            })
            .unwrap();
        assert_eq!(
            answer.writes[0]["result"],
            json!({"answers":{"scope":{"answers":["One"]}}})
        );
    }
}
