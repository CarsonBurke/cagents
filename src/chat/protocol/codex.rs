//! Codex 0.153.4 app-server JSON-RPC, not TUI output.
//! Wire contracts: `codex app-server generate-json-schema --experimental`,
//! https://github.com/openai/codex/tree/rust-v0.153.4/codex-rs/app-server-protocol
//! Pending requests are replayed to joining connections with the original ID;
//! the first answer wins (app-server/src/outgoing_message.rs). Never auto-answer.
use super::{Adapter, ProcessSpec, Step, TransportSpec};
use crate::chat::{
    Answer, Attachment, Capabilities, ChangeKind, Choice, Command, CommandAction, Event, Field,
    FileChange, Item, ItemContent, Launch, Model, Phase, PlanState, PlanStep, PromptMode, Request,
    Role, SearchResult, Skill, ToolStatus, Usage,
};
use serde_json::{Value, json};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::{collections::BTreeMap, path::PathBuf};

const MAX_PENDING: usize = 128;
const MAX_MODELS: usize = 512;
const MAX_ACTIVE_ITEMS: usize = 128;
const MAX_ITEM_BYTES: usize = 512 * 1024;
const OUTPUT_TRUNCATED: &str = "\n[Output truncated for display]";

fn append_display_output(output: &mut String, delta: &str, limit: usize) -> bool {
    if (output.ends_with(OUTPUT_TRUNCATED) && output.len().saturating_add(3) >= limit)
        || delta.is_empty()
    {
        return false;
    }
    if output.len().saturating_add(delta.len()) <= limit {
        output.push_str(delta);
        return true;
    }
    let budget = limit.saturating_sub(OUTPUT_TRUNCATED.len());
    let end = output.floor_char_boundary(budget.min(output.len()));
    output.truncate(end);
    let end = delta.floor_char_boundary(budget.saturating_sub(output.len()).min(delta.len()));
    output.push_str(&delta[..end]);
    if limit >= OUTPUT_TRUNCATED.len() {
        output.push_str(OUTPUT_TRUNCATED);
    }
    true
}
/// How many skills one conversation will carry into the composer's completion
/// list. A directory tree with a pathological number of skill files must not
/// grow the snapshot without bound.
const MAX_SKILLS: usize = 256;
/// How much of a skill's name is carried. A name is the skill file's own, so
/// it is provider-controlled text of no guaranteed length, and a completion
/// row has no use for more than this.
const MAX_SKILL_NAME: usize = 64;

fn remote_socket(address: &str) -> Result<Option<PathBuf>, String> {
    let path = address.strip_prefix("unix://").ok_or(
        "Native Codex chat can verify only a local unix:// shared daemon; ws:// and wss:// remote endpoints are unsupported and will not be replaced by a local daemon",
    )?;
    Ok((!path.is_empty()).then(|| PathBuf::from(path)))
}

/// Record a provider-confirmed rotation in the recipe: rewrites the `resume`
/// thread ID `create()` validates so a checkpointed launch stays
/// self-consistent. Only call with the live provider's own identity.
pub fn align_resume_identity(arguments: &mut Vec<String>, id: &str) -> Result<(), String> {
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == "resume" {
            index += 1;
            *arguments
                .get_mut(index)
                .ok_or("Codex resume requires an exact thread ID")? = id.to_string();
        }
        index += 1;
    }
    Ok(())
}

pub fn create(launch: &Launch) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    create_inner(launch, None)
}

pub(crate) fn create_with_listener(
    launch: &Launch,
    socket: PathBuf,
) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    create_inner(launch, Some(socket))
}

fn create_inner(
    launch: &Launch,
    owned_socket: Option<PathBuf>,
) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
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
    let mut shared = false;
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
                shared = true;
                explicit_socket =
                    remote_socket(args.next().ok_or("Missing Codex --remote address")?)?;
            }
            _ if arg.starts_with("--remote=") => {
                shared = true;
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
            "--profile" | "-p" | "--config" | "-c" | "--add-dir" | "--remote-auth-token-env"
                if launch.session_id.is_some() =>
            {
                // The exact thread is already configured on the shared
                // server. Keep its live permissions/model, not TUI defaults.
                args.next()
                    .ok_or_else(|| format!("Missing value for Codex {arg}"))?;
            }
            "--search" if launch.session_id.is_some() => {}
            "--dangerously-bypass-approvals-and-sandbox" | "--yolo" => {
                // A new thread has no live configuration to protect, so the
                // terminal's own bypass carries over as this thread's
                // equivalent settings. Refusing it stranded a terminal started
                // in that mode with no way into a native conversation.
                if launch.session_id.is_none() {
                    start["sandbox"] = json!("danger-full-access");
                    start["approvalPolicy"] = json!("never");
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
        .map(PathBuf::from)
        .or_else(|| {
            spec.environment
                .get("HOME")
                .cloned()
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
    // Joining a thread that already exists means joining whichever process
    // owns it, so it needs the shared control socket. A brand-new
    // conversation owns nothing, so it runs as this application's own `codex
    // app-server` child over stdio: demanding the shared daemon there made a
    // fresh native chat impossible on installs that cannot host that daemon,
    // and bought no safety, because there is no existing thread to collide
    // with. An explicitly named socket is still honoured for a fresh thread,
    // because naming one is a request to use that server.
    let mut alias = None;
    if let Some(socket) = owned_socket {
        spec.arguments = vec![
            "app-server".into(),
            "--listen".into(),
            format!("unix://{}", socket.display()),
        ];
        spec.transport = TransportSpec::OwnedUnixWebSocket { socket };
    } else if shared || launch.session_id.is_some() || explicit_socket.is_some() {
        let socket = explicit_socket
            .unwrap_or_else(|| home.join("app-server-control/app-server-control.sock"));
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
        // Linux sun_path is 108 bytes. The symlink resolves to this exact
        // verified socket, not a discovered/newest daemon or a different
        // CODEX_HOME.
        let connect_socket = if socket.as_os_str().as_encoded_bytes().len() >= 108 {
            let directory = PathBuf::from("/tmp")
                .join(format!("cagents-codex-{}", uuid::Uuid::new_v4().simple()));
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
    } else {
        spec.arguments.clear();
        spec.arguments.push("app-server".into());
    }
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
            skills_stale: false,
            materialized: launch.session_id.is_some(),
        }),
    ))
}

#[derive(Clone, Copy)]
enum Pending {
    Initialize,
    Attach,
    Models,
    Skills,
    History,
    Turn,
    /// Compaction rewrites the thread's history and its context occupancy, so
    /// its acknowledgement is what a re-read hangs off: nothing else on the
    /// wire announces that the figures a reader was looking at have moved.
    Compact,
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
    /// Whether the catalog was invalidated again while a request for it was
    /// still outstanding, so exactly one refetch follows that answer.
    skills_stale: bool,
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
    /// Ask the account's server which skills apply to this conversation's
    /// directory.
    ///
    /// The catalog is per directory, not per account, so it is requested with
    /// the thread's own working directory and requested again whenever the
    /// server reports that the watched skill files changed.
    fn skills(&mut self, step: &mut Step) -> Result<(), String> {
        // One catalog request at a time. A watcher over a churning skills
        // directory sends a burst of invalidations, and a request per
        // invalidation would fill the pending map and fail the conversation
        // over a completion list.
        if self
            .pending
            .values()
            .any(|pending| matches!(pending, Pending::Skills))
        {
            self.skills_stale = true;
            return Ok(());
        }
        self.skills_stale = false;
        let cwd = self.cwd.clone();
        self.call(step, "skills/list", json!({"cwds":[cwd]}), Pending::Skills)
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
    /// Re-reads the thread's metadata and its history.
    ///
    /// A history load already in flight will itself deliver the current
    /// thread, so it is left running rather than refused. That matters
    /// because this is also the path a compaction acknowledgement takes, and
    /// an acknowledgement must not turn into an error the reader has to read.
    fn reread(&mut self, step: &mut Step) -> Result<(), String> {
        self.call(
            step,
            "thread/read",
            json!({"threadId":self.thread,"includeTurns":false}),
            Pending::Other,
        )?;
        if self.history_loading {
            return Ok(());
        }
        self.history(step, None)
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
                    if active && self.busy && streams_updates(&item.content) {
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
            if !crate::chat::is_image_path(&path) {
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
                // A question whose blocking flag never arrived is described as
                // the blocking kind, because overstating a question's urgency
                // costs less than ending the session it was asked in.
                description = if params["isBlocking"].as_bool().unwrap_or(true) {
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
                self.call(&mut step, "thread/compact/start", json!({"threadId":self.thread}), Pending::Compact)?;
            }
            Command::Refresh => {
                if self.history_loading { return Err("Codex history is already loading".into()); }
                if self.pending.len() > MAX_PENDING - 2 { return Err("Too many outstanding Codex requests".into()); }
                self.reread(&mut step)?;
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
                if matches!(pending, Pending::Skills) {
                    // An app-server without a skills catalog answers with an
                    // error, and a composer that offers no completions is not
                    // worth putting a failure in the transcript over.
                    return Ok(step);
                }
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
                    crate::agent::observe::socket::verify_account(result, &self.expected_home)?;
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
                    self.skills(&mut step)?;
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
                    step.events
                        .push(Event::Waiting(waiting_on_user(&thread["status"])));
                    step.events.push(Event::Session {
                        id: id.into(),
                        title: thread["name"].as_str().map(str::to_owned),
                    });
                    step.events.push(Event::Status {
                        busy: self.busy,
                        detail: "Connected to Codex".into(),
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
                        // A catalog entry that does not describe its efforts,
                        // or does not name itself for the picker, is still a
                        // model the user can select, so the unreadable parts
                        // are dropped rather than the whole catalog with the
                        // session it arrived in.
                        let reasoning: Vec<String> = value["supportedReasoningEfforts"]
                            .as_array()
                            .map(|efforts| {
                                efforts
                                    .iter()
                                    .filter_map(|effort| effort["reasoningEffort"].as_str())
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let model = Model {
                            id: id.clone(),
                            label: value["displayName"].as_str().unwrap_or(&id).into(),
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
                Pending::Skills => {
                    let mut skills: Vec<Skill> = Vec::new();
                    // The cap has to stop the outer walk as well: a catalog
                    // split across many directories would otherwise add one
                    // more skill per directory after the cap was reached.
                    'catalog: for entry in result["data"].as_array().unwrap_or(&Vec::new()) {
                        for value in entry["skills"].as_array().unwrap_or(&Vec::new()) {
                            if skills.len() >= MAX_SKILLS {
                                break 'catalog;
                            }
                            let Some(name) = value["name"].as_str().filter(|n| !n.is_empty())
                            else {
                                continue;
                            };
                            if value["enabled"].as_bool() == Some(false)
                                || skills.iter().any(|skill| skill.name == name)
                            {
                                continue;
                            }
                            // A name is a file name in the working tree, so it
                            // is as much provider-controlled text as the
                            // description and is bounded the same way.
                            let name = name.chars().take(MAX_SKILL_NAME).collect::<String>();
                            // The short description is the one written for a
                            // picker; the long one is written for the model
                            // and runs to paragraphs.
                            let description = value["interface"]["shortDescription"]
                                .as_str()
                                .or_else(|| value["shortDescription"].as_str())
                                .or_else(|| value["description"].as_str())
                                .unwrap_or_default();
                            skills.push(Skill {
                                description: description
                                    .lines()
                                    .next()
                                    .unwrap_or_default()
                                    .chars()
                                    .take(160)
                                    .collect(),
                                // Codex acts on a skill the message names with
                                // a leading dollar sign. It has no command for
                                // one, so there is nothing to dispatch.
                                invocation: format!("${name}"),
                                name,
                            });
                        }
                    }
                    step.events.push(Event::Skills(skills));
                    if std::mem::take(&mut self.skills_stale) {
                        self.skills(&mut step)?;
                    }
                }
                Pending::History => self.page(result, &mut step)?,
                Pending::Turn => {
                    self.materialized = true;
                    self.turn = Some(string(&result["turn"], "id")?.into());
                    self.busy = true;
                    step.events.push(Event::Waiting(false));
                    // Preserve selection until an explicit user change. No settings
                    // update RPC is sent while another client's turn is running.
                    step.events.push(Event::Status {
                        busy: true,
                        detail: "Codex turn started".into(),
                    });
                }
                // The one visible effect of compaction is the context figure,
                // and no notification announces that it has moved, so the
                // acknowledgement re-reads the thread the way a refresh does.
                // Without this the reader pressed a control and the gauge it
                // exists to move kept its old reading until some later frame
                // happened to carry a new one.
                Pending::Compact => self.reread(&mut step)?,
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
                step.events
                    .push(Event::Waiting(waiting_on_user(&params["status"])));
                if !self.busy {
                    self.turn = None;
                }
                step.events.push(Event::Status {
                    busy: self.busy,
                    detail: status_detail(&params["status"]),
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
                step.events.push(Event::Waiting(false));
                self.turn = if self.busy {
                    Some(string(turn, "id")?.into())
                } else {
                    None
                };
                for value in array(turn, "items")? {
                    for item in parse_item(value, !self.busy, string(turn, "id")?)? {
                        if self.busy && streams_updates(&item.content) {
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
                    detail: turn_detail(&turn["status"]),
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
                    } else if streams_updates(&item.content) {
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
                // A delta for an item this adapter is not tracking belongs to
                // an item that already completed, or to one that never
                // streamed, and losing the tail of its output is better than
                // ending the session over a delta nothing can render.
                if let Some(item) = self.active_items.get_mut(&id) {
                    let bytes = item.content.byte_len();
                    // A fileChange's content is its patch rather than a stream, so the
                    // deprecated textual delta that used to accompany one has nowhere to land.
                    if let ItemContent::Tool { output, .. } | ItemContent::Command { output, .. } =
                        &mut item.content
                    {
                        // An MCP progress notification only carries a message
                        // when the server wrote one.
                        let delta = if method.ends_with("progress") {
                            params["message"].as_str().unwrap_or_default()
                        } else {
                            string(params, "delta")?
                        };
                        let limit =
                            MAX_ITEM_BYTES.saturating_sub(bytes.saturating_sub(output.len()));
                        if append_display_output(output, delta, limit) {
                            step.events.push(Event::Item(item.clone()));
                        }
                    }
                }
            }
            "item/fileChange/patchUpdated" => {
                let id = item_key(string(params, "turnId")?, string(params, "itemId")?);
                // The patch is parsed only once there is an item to hold it, so
                // a notification for an item this adapter never tracked cannot
                // fail the session over a patch nothing would render.
                if let Some(item) = self.active_items.get_mut(&id) {
                    if let ItemContent::Changes { changes, .. } = &mut item.content {
                        *changes = file_changes(params)?;
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
                at: None,
            })),
            "turn/plan/updated" => step.events.push(Event::Plan {
                steps: plan_steps(&params["plan"]),
            }),
            "thread/queue/changed" => step.events.push(Event::Status {
                busy: self.busy,
                detail: "Codex's native queue changed".into(),
            }),
            // The catalog is a cache the server watches for itself, so a
            // change notification is an invalidation and nothing more.
            "skills/changed" => self.skills(&mut step)?,
            "item/reasoning/summaryPartAdded" | "thread/compacted" => {}
            // Auxiliary app-server notifications are not transcript data.
            // Unknown item types are shown as tool disclosures and unknown
            // requests are visibly rejected above.
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
        step.events
            .push(Event::Waiting(waiting_on_user(&thread["status"])));
        step.events.push(Event::Session {
            id: id.into(),
            title: thread["name"].as_str().map(str::to_owned),
        });
        step.events.push(Event::Status {
            busy: self.busy,
            detail: status_detail(&thread["status"]),
        });
        step.events.push(Event::Capabilities(self.capabilities()));
        Ok(())
    }
}
fn waiting_on_user(status: &Value) -> bool {
    status["type"] == "active"
        && status["activeFlags"].as_array().is_some_and(|flags| {
            flags.iter().any(|flag| {
                matches!(
                    flag.as_str(),
                    Some("waitingOnApproval" | "waitingOnUserInput")
                )
            })
        })
}

/// A thread status as a line a reader can act on, or nothing where the state
/// speaks for itself.
///
/// The protocol's status is a tagged object, and handing it to the status line
/// unrendered put `{"type":"idle"}` on screen and into the transcript as a
/// notice. Idle produces no text at all, because a resting session has nothing
/// to report and the status line already says as much by being quiet.
fn status_detail(status: &Value) -> String {
    let flag = |name: &str| {
        status["activeFlags"]
            .as_array()
            .is_some_and(|flags| flags.iter().any(|flag| flag.as_str() == Some(name)))
    };
    match status["type"].as_str() {
        Some("active") if flag("waitingOnApproval") => "Waiting for your approval".into(),
        Some("active") if flag("waitingOnUserInput") => "Waiting for your answer".into(),
        Some("active") => String::new(),
        Some("notLoaded") => "Thread not loaded".into(),
        Some("systemError") => "Codex reported a system error".into(),
        _ => String::new(),
    }
}

/// A turn status as a line a reader can act on.
///
/// The successful states are silent: an in-progress turn is already drawn as
/// activity and a completed one is drawn by its answer, so naming either one
/// only spends a row of the status line on a word the reader can see.
fn turn_detail(status: &Value) -> String {
    match status.as_str() {
        Some("interrupted") => "Turn interrupted".into(),
        Some("failed") => "Turn failed".into(),
        _ => String::new(),
    }
}

/// A published plan as its steps.
///
/// The payload is accepted both as the bare array of steps and as the object
/// that wraps one, because Codex's plan tool carries an explanation beside the
/// steps and the notification has changed which of the two it sends. A step
/// with no recognizable text is dropped rather than pinned as a blank row.
fn plan_steps(value: &Value) -> Vec<PlanStep> {
    let steps = value
        .as_array()
        .or_else(|| value["plan"].as_array())
        .or_else(|| value["steps"].as_array());
    steps
        .into_iter()
        .flatten()
        .filter_map(|step| {
            let text = step
                .as_str()
                .or_else(|| step["step"].as_str())
                .or_else(|| step["text"].as_str())
                .or_else(|| step["description"].as_str())?
                .trim();
            if text.is_empty() {
                return None;
            }
            Some(PlanStep {
                text: text.to_owned(),
                status: match step["status"].as_str() {
                    Some("completed" | "complete" | "done") => PlanState::Done,
                    Some("in_progress" | "inProgress" | "active") => PlanState::Active,
                    _ => PlanState::Pending,
                },
            })
        })
        .collect()
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
/// Items the app-server keeps writing into after announcing them, which is why
/// they are retained and re-emitted rather than rendered once and forgotten.
fn streams_updates(content: &ItemContent) -> bool {
    matches!(
        content,
        ItemContent::Tool { .. } | ItemContent::Command { .. } | ItemContent::Changes { .. }
    )
}
/// Codex leaves `phase` null for models that do not classify their messages,
/// and an unclassified message has to be treated as the turn's answer or the
/// answer would be hidden for those models.
fn message_phase(value: &Value) -> Phase {
    match value["phase"].as_str() {
        Some("commentary") => Phase::Commentary,
        _ => Phase::Final,
    }
}
/// Codex's own best-effort parse of a shell command, which may decompose into
/// several actions because a command can be a pipeline. The first classified
/// action is what the invocation was for; `unknown` entries carry nothing the
/// view cannot already read off the command itself.
fn command_intent(actions: &Value) -> (CommandAction, String) {
    let Some(actions) = actions.as_array() else {
        return (CommandAction::Run, String::new());
    };
    for action in actions {
        let path = action["path"].as_str();
        match action["type"].as_str() {
            Some("read") => {
                return (
                    CommandAction::Read,
                    path.map(|path| format!("Read {path}")).unwrap_or_default(),
                );
            }
            Some("search") => {
                return (
                    CommandAction::Search,
                    match (action["query"].as_str(), path) {
                        (Some(query), Some(path)) => format!("Search {query} in {path}"),
                        (Some(query), None) => format!("Search {query}"),
                        (None, Some(path)) => format!("Search {path}"),
                        (None, None) => String::new(),
                    },
                );
            }
            Some("listFiles") => {
                return (
                    CommandAction::List,
                    path.map(|path| format!("List {path}")).unwrap_or_default(),
                );
            }
            _ => {}
        }
    }
    (CommandAction::Run, String::new())
}
/// Codex keys a patch by the file it read, so an update's `move_path` is where
/// the file ended up while the entry's own `path` is where it came from.
fn file_changes(value: &Value) -> Result<Vec<FileChange>, String> {
    let entries = array(value, "changes")?;
    let mut changes = Vec::with_capacity(entries.len());
    for change in entries {
        let original = string(change, "path")?;
        let descriptor = &change["kind"];
        let (path, kind) = match descriptor["type"].as_str().unwrap_or_default() {
            "add" => (original.to_owned(), ChangeKind::Added),
            "delete" => (original.to_owned(), ChangeKind::Deleted),
            "update" => match descriptor["move_path"].as_str() {
                Some(moved) => (
                    moved.to_owned(),
                    ChangeKind::Renamed {
                        from: original.into(),
                    },
                ),
                None => (original.to_owned(), ChangeKind::Updated),
            },
            // A kind this build does not know, and a kind an entry never
            // stated, is still a file change, and rendering it as an update
            // keeps the transcript alive where rejecting the patch would end
            // the session.
            _ => (original.to_owned(), ChangeKind::Updated),
        };
        changes.push(FileChange {
            path,
            kind,
            // Reporting an absent patch as an empty one would render a real
            // edit as a change that touched nothing.
            diff: string(change, "diff")?.into(),
        });
    }
    Ok(changes)
}
fn parse_item(value: &Value, done: bool, turn: &str) -> Result<Vec<Item>, String> {
    let scoped_id = item_key(turn, string(value, "id")?);
    let id = scoped_id.as_str();
    let kind = string(value, "type")?;
    // Only an assistant message has a phase: user input, reasoning and system
    // notices are never the answer a turn was asked for.
    let text = |id: String, text: String, thinking: bool, role: Role| Item {
        id,
        role,
        content: ItemContent::Text {
            text,
            thinking,
            phase: Phase::Final,
        },
        at: None,
    };
    // A non-zero exit code is deliberately not a failure here. `grep` with no
    // matches exits 1, `git diff --quiet` exits 1 when there is a diff, and a
    // test runner exits non-zero to report the result it was asked for: all of
    // them are the step doing its job. The code itself reaches the step's
    // detail line, which is where a reader who needs it looks, and only what
    // Codex itself reports as an error - a failed status, an error payload, an
    // explicit `success: false` - marks the step failed.
    let status = match value["status"].as_str() {
        _ if value["success"] == false || value["error"].is_object() => ToolStatus::Failed,
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
            at: None,
        }]
    };
    let mut items = match kind {
        "agentMessage" | "plan" => vec![Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Text {
                text: string(value, "text")?.into(),
                thinking: false,
                phase: message_phase(value),
            },
            at: None,
        }],
        "userMessage" => {
            let mut items = Vec::new();
            for (index, content) in array(value, "content")?.iter().enumerate() {
                let part_id = format!("{id}/input/{index}");
                match content["type"].as_str().unwrap_or_default() {
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
                        at: None,
                    }),
                    // A mention, skill or audio part has no rendering of its
                    // own, and neither has a part type this build cannot name,
                    // whether because a later Codex introduced it or because
                    // the part never stated one. Showing the payload keeps the
                    // user's own input in the transcript, where rejecting it
                    // would end the session that received it.
                    _ => items.push(text(part_id, content.to_string(), false, Role::User)),
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
        "commandExecution" => {
            let (action, label) = command_intent(&value["commandActions"]);
            vec![Item {
                id: id.into(),
                role: Role::Assistant,
                content: ItemContent::Command {
                    label,
                    command: string(value, "command")?.into(),
                    working_directory: string(value, "cwd")?.into(),
                    action,
                    status,
                    exit_code: value["exitCode"]
                        .as_i64()
                        .and_then(|code| i32::try_from(code).ok()),
                    duration_ms: value["durationMs"].as_u64(),
                    output: value["aggregatedOutput"].as_str().unwrap_or("").into(),
                },
                at: None,
            }]
        }
        "fileChange" => vec![Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Changes {
                changes: file_changes(value)?,
                status,
            },
            at: None,
        }],
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
        "webSearch" => vec![Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::WebSearch {
                query: string(value, "query")?.into(),
                // Results cross the app-server boundary as opaque JSON, so a
                // result type without the fields the model names still
                // contributes whatever it does carry.
                results: value["results"]
                    .as_array()
                    .map(|results| {
                        results
                            .iter()
                            .map(|result| SearchResult {
                                title: result["title"].as_str().unwrap_or("").into(),
                                url: result["url"].as_str().unwrap_or("").into(),
                                snippet: result["snippet"].as_str().unwrap_or("").into(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            at: None,
        }],
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
            at: None,
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
        // Codex's own auxiliary items, `sleep` and `subAgentActivity` among
        // them, and any item type a later Codex introduces, still belong to the
        // turn. The tool disclosure exists to keep an unrecognized item
        // renderable rather than ending the session over it.
        other => tool(other.into(), String::new(), value.to_string()),
    };
    for item in &mut items {
        let bytes = item.content.byte_len();
        if bytes > MAX_ITEM_BYTES {
            if let ItemContent::Tool { output, .. } | ItemContent::Command { output, .. } =
                &mut item.content
            {
                let limit = MAX_ITEM_BYTES.saturating_sub(bytes.saturating_sub(output.len()));
                // Completed items replace streamed items, so apply the same bound.
                let full = std::mem::take(output);
                append_display_output(output, &full, limit);
            }
        }
        if item.content.byte_len() > MAX_ITEM_BYTES {
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

    #[test]
    fn an_explicit_default_remote_starts_new_threads_on_the_shared_server() {
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join("app-server-control");
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("app-server-control.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let launch = Launch {
            provider: crate::agent::Provider::Codex,
            session_id: None,
            recipe: crate::model::LaunchRecipe {
                program: "codex".into(),
                arguments: vec!["--remote".into(), "unix://".into()],
                working_directory: home.path().into(),
                environment: BTreeMap::from([(
                    "CODEX_HOME".into(),
                    home.path().to_string_lossy().into_owned(),
                )]),
            },
        };
        let (spec, mut adapter) = create(&launch).unwrap();
        assert!(matches!(
            spec.transport,
            TransportSpec::UnixWebSocket { .. }
        ));
        assert!(spec.arguments.is_empty());
        let initialize = adapter.initialize().unwrap();
        let result = adapter.receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":home.path()}})).unwrap();
        assert!(
            result
                .writes
                .iter()
                .any(|frame| frame["method"] == "thread/start")
        );
        drop(_listener);
        std::fs::remove_file(socket).unwrap();
        assert!(
            create(&launch).is_err(),
            "an unavailable selected server must not create a private thread"
        );
    }

    #[test]
    fn align_resume_identity_rewrites_resume_thread() {
        let mut arguments = vec![
            "app-server".to_string(),
            "resume".to_string(),
            "old".to_string(),
        ];
        align_resume_identity(&mut arguments, "new").unwrap();
        assert_eq!(arguments, vec!["app-server", "resume", "new"]);
        assert!(align_resume_identity(&mut vec!["resume".to_string()], "new").is_err());
    }

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
            skills_stale: false,
            materialized: true,
        }
    }

    #[test]
    fn joins_exact_thread_without_settings_or_history_overrides() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("shared.sock");
        let _server = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let launch = Launch {
            provider: crate::agent::Provider::Codex,
            session_id: Some("thread-exact".into()),
            recipe: crate::model::LaunchRecipe {
                program: "codex".into(),
                arguments: [
                    "resume",
                    "thread-exact",
                    "--remote",
                    &format!("unix://{}", socket.display()),
                    "--profile",
                    "work",
                    "--config",
                    "model=\"other-model\"",
                    "--model",
                    "other-model",
                    "--search",
                    "--dangerously-bypass-approvals-and-sandbox",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                working_directory: directory.path().into(),
                environment: BTreeMap::from([(
                    "CODEX_HOME".into(),
                    directory.path().to_str().unwrap().into(),
                )]),
            },
        };
        let (spec, mut adapter) = create(&launch).unwrap();
        assert!(matches!(
            spec.transport,
            TransportSpec::UnixWebSocket { .. }
        ));
        let initialize = adapter.initialize().unwrap();
        let step = adapter.receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":directory.path()}})).unwrap();
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
    fn a_new_thread_carries_the_terminal_bypass_into_its_own_settings() {
        let directory = tempfile::tempdir().unwrap();
        let launch = Launch {
            provider: crate::agent::Provider::Codex,
            session_id: None,
            recipe: crate::model::LaunchRecipe {
                program: "codex".into(),
                arguments: ["--dangerously-bypass-approvals-and-sandbox"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                working_directory: directory.path().into(),
                environment: BTreeMap::from([(
                    "CODEX_HOME".into(),
                    directory.path().to_str().unwrap().into(),
                )]),
            },
        };
        let (spec, mut adapter) = create(&launch).unwrap();
        assert_eq!(spec.arguments, vec!["app-server"]);
        let initialize = adapter.initialize().unwrap();
        let step = adapter.receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":directory.path()}})).unwrap();
        let start = step
            .writes
            .iter()
            .find(|frame| frame["method"] == "thread/start")
            .unwrap();
        assert_eq!(start["params"]["sandbox"], json!("danger-full-access"));
        assert_eq!(start["params"]["approvalPolicy"], json!("never"));
    }

    /// The completion list is bounded whatever the account's skill tree looks
    /// like. A catalog arrives grouped by directory, so a cap that stopped only
    /// the walk within one group grew by one skill per directory.
    #[test]
    fn a_catalog_split_across_directories_still_stops_at_the_cap() {
        let mut adapter = adapter();
        let initialize = adapter.initialize().unwrap();
        let setup = adapter
            .receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":"/tmp"}}))
            .unwrap();
        let list = setup
            .writes
            .iter()
            .find(|frame| frame["method"] == "skills/list")
            .unwrap();
        let data = (0..MAX_SKILLS * 2)
            .map(|group| json!({"skills":[{"name":format!("skill-{group}")}]}))
            .collect::<Vec<_>>();
        let answered = adapter
            .receive(json!({"id":list["id"],"result":{"data":data}}))
            .unwrap();
        let skills = answered
            .events
            .iter()
            .find_map(|event| match event {
                Event::Skills(skills) => Some(skills),
                _ => None,
            })
            .unwrap();
        assert_eq!(skills.len(), MAX_SKILLS);
        assert_eq!(skills[0].invocation, "$skill-0");
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
    fn resumed_step_and_final_history_use_same_turn_scoped_identity() {
        let mut adapter = adapter();
        adapter.busy = true;
        let item = json!({"id":"0","type":"commandExecution","command":"echo hello","cwd":"/tmp","status":"inProgress","aggregatedOutput":"hel"});
        let mut page = Step::default();
        adapter.page(&json!({"data":[{"id":"turn-a","status":"inProgress","items":[item]}],"nextCursor":"older"}),&mut page).unwrap();
        let delta = adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"turn-a","itemId":"0","delta":"lo"}})).unwrap();
        assert!(
            matches!(&delta.events[0],Event::Item(Item { id,content:ItemContent::Command { output,.. },.. }) if id == "turn-a/0" && output == "hello")
        );
        let completed = json!({"id":"0","type":"agentMessage","text":"answer"});
        let one = parse_item(&completed, true, "turn-a").unwrap();
        let two = parse_item(&completed, true, "turn-b").unwrap();
        assert_ne!(one[0].id, two[0].id);
        let live = adapter.receive(json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact","turnId":"turn-a","itemId":"0","delta":"answer"}})).unwrap();
        assert!(matches!(&live.events[0],Event::TextDelta { id,.. } if id == &one[0].id));
    }

    #[test]
    fn oversized_tool_output_keeps_stream_and_completion_alive() {
        let mut adapter = adapter();
        adapter.busy = true;
        let mut item = json!({"id":"large","type":"commandExecution","command":"cat data","cwd":"/tmp","status":"inProgress","aggregatedOutput":""});
        adapter.receive(json!({"method":"item/started","params":{"threadId":"thread-exact","turnId":"turn","item":item}})).unwrap();
        let delta = "🦀".repeat(MAX_ITEM_BYTES);
        let streamed = adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"turn","itemId":"large","delta":delta}})).unwrap();
        let Event::Item(streamed) = &streamed.events[0] else {
            panic!("missing streamed item")
        };
        assert!(streamed.content.byte_len() <= MAX_ITEM_BYTES);
        assert!(
            matches!(&streamed.content, ItemContent::Command { output, .. } if output.ends_with(OUTPUT_TRUNCATED))
        );
        item["status"] = json!("completed");
        item["aggregatedOutput"] = json!(delta);
        let completed = adapter.receive(json!({"method":"item/completed","params":{"threadId":"thread-exact","turnId":"turn","item":item}})).unwrap();
        let Event::Item(completed) = &completed.events[0] else {
            panic!("missing completed item")
        };
        assert!(completed.content.byte_len() <= MAX_ITEM_BYTES);
        assert!(matches!(
            &completed.content,
            ItemContent::Command {
                status: ToolStatus::Succeeded,
                ..
            }
        ));
        assert!(adapter.active_items.is_empty());
        adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"turn","status":"completed","items":[]}}})).unwrap();
        assert!(!adapter.busy);
    }

    #[test]
    fn file_change_rename_points_at_the_destination_and_keeps_the_source() {
        let item = json!({"id":"0","type":"fileChange","status":"completed","changes":[
            {"path":"src/old.rs","kind":{"type":"update","move_path":"src/new.rs"},"diff":"@@ -1 +1 @@\n-old\n+new\n"},
            {"path":"src/kept.rs","kind":{"type":"update","move_path":null},"diff":"@@ -2 +2 @@\n"},
            {"path":"src/gone.rs","kind":{"type":"delete"},"diff":"-gone\n"},
            {"path":"src/fresh.rs","kind":{"type":"add"},"diff":"+fresh\n"}
        ]});
        let items = parse_item(&item, true, "turn").unwrap();
        let ItemContent::Changes { changes, status } = &items[0].content else {
            panic!("A Codex fileChange must carry its patch, not a rendering of it");
        };
        assert_eq!(*status, ToolStatus::Succeeded);
        assert_eq!(
            changes[0],
            FileChange {
                path: "src/new.rs".into(),
                kind: ChangeKind::Renamed {
                    from: "src/old.rs".into()
                },
                diff: "@@ -1 +1 @@\n-old\n+new\n".into(),
            }
        );
        assert_eq!(changes[1].kind, ChangeKind::Updated);
        assert_eq!(changes[2].kind, ChangeKind::Deleted);
        assert_eq!(changes[3].kind, ChangeKind::Added);
        let unknown = parse_item(&json!({"id":"0","type":"fileChange","status":"completed","changes":[{"path":"p","kind":{"type":"chmod"},"diff":"@@ -1 +1 @@\n"}]}), true, "turn").unwrap();
        let ItemContent::Changes { changes, .. } = &unknown[0].content else {
            panic!("A file change kind this build does not know is still a file change");
        };
        assert_eq!(changes[0].kind, ChangeKind::Updated);
    }

    #[test]
    fn an_unrecognized_item_part_or_untracked_patch_cannot_end_the_session() {
        let future = json!({"id":"0","type":"quantumThought","status":"completed","why":"new"});
        let items = parse_item(&future, true, "turn").unwrap();
        assert!(matches!(&items[0].content,
            ItemContent::Tool { name, output, status: ToolStatus::Succeeded, .. }
            if name == "quantumThought" && output.contains("quantumThought")));
        let part = json!({"id":"1","type":"userMessage","content":[{"type":"hologram","n":3}]});
        let items = parse_item(&part, true, "turn").unwrap();
        assert!(matches!(&items[0].content,
            ItemContent::Text { text, .. } if text.contains("hologram")));
        assert_eq!(items[0].role, Role::User);
        let mut adapter = adapter();
        let ignored = adapter.receive(json!({"method":"item/fileChange/patchUpdated","params":{"threadId":"thread-exact","turnId":"turn-a","itemId":"0","changes":"not an array"}})).unwrap();
        assert!(ignored.events.is_empty());
    }

    #[test]
    fn command_actions_classify_the_command_and_name_its_target() {
        let read = json!({"id":"0","type":"commandExecution","command":"sed -n '1,20p' src/app.rs","cwd":"/repo","status":"completed","exitCode":0,"durationMs":12,"aggregatedOutput":"fn main() {}","commandActions":[{"type":"unknown","command":"cd /repo"},{"type":"read","command":"sed -n '1,20p' src/app.rs","name":"app.rs","path":"src/app.rs"},{"type":"search","command":"rg main","query":"main","path":"src"}]});
        let items = parse_item(&read, true, "turn").unwrap();
        let ItemContent::Command {
            label,
            command,
            working_directory,
            action,
            exit_code,
            duration_ms,
            output,
            ..
        } = &items[0].content
        else {
            panic!("A Codex commandExecution must map to a command");
        };
        assert_eq!(*action, CommandAction::Read);
        assert_eq!(label, "Read src/app.rs");
        assert_eq!(command, "sed -n '1,20p' src/app.rs");
        assert_eq!(working_directory, "/repo");
        assert_eq!(*exit_code, Some(0));
        assert_eq!(*duration_ms, Some(12));
        assert_eq!(output, "fn main() {}");
        let unclassified = json!({"id":"1","type":"commandExecution","command":"make release","cwd":"/repo","status":"inProgress","commandActions":[{"type":"unknown","command":"make release"}]});
        let items = parse_item(&unclassified, false, "turn").unwrap();
        let ItemContent::Command {
            label,
            action,
            status,
            exit_code,
            duration_ms,
            ..
        } = &items[0].content
        else {
            panic!("A Codex commandExecution must map to a command");
        };
        assert_eq!(*action, CommandAction::Run);
        assert!(label.is_empty());
        assert_eq!(*status, ToolStatus::Running);
        assert_eq!(*exit_code, None);
        assert_eq!(*duration_ms, None);
    }

    #[test]
    fn a_non_zero_exit_code_is_a_result_rather_than_a_failure() {
        let no_matches = json!({"id":"0","type":"commandExecution","command":"grep -n todo src","cwd":"/repo","status":"completed","exitCode":1,"durationMs":8,"aggregatedOutput":""});
        let items = parse_item(&no_matches, true, "turn").unwrap();
        let ItemContent::Command {
            status, exit_code, ..
        } = &items[0].content
        else {
            panic!("A Codex commandExecution must map to a command");
        };
        assert_eq!(
            *status,
            ToolStatus::Succeeded,
            "a grep with no matches exits 1 and has not failed at anything"
        );
        assert_eq!(*exit_code, Some(1), "the code itself stays visible");
        let reported = json!({"id":"1","type":"commandExecution","command":"cargo build","cwd":"/repo","status":"failed","exitCode":101});
        let items = parse_item(&reported, true, "turn").unwrap();
        let ItemContent::Command { status, .. } = &items[0].content else {
            panic!("A Codex commandExecution must map to a command");
        };
        assert_eq!(
            *status,
            ToolStatus::Failed,
            "what Codex itself calls a failure is still a failure"
        );
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

    /// Compaction's only visible effect is the context figure the reader
    /// pressed the control to move. Nothing on the wire announces that it
    /// moved, so the acknowledgement has to re-read the thread; without this
    /// the gauge kept its old reading until some later frame happened to
    /// carry a new one.
    #[test]
    fn a_compaction_acknowledgement_re_reads_the_thread() {
        let mut adapter = adapter();
        let requested = adapter.command(Command::Compact).unwrap();
        let start = requested
            .writes
            .iter()
            .find(|frame| frame["method"] == "thread/compact/start")
            .expect("compaction is requested");
        let acknowledged = adapter
            .receive(json!({"id":start["id"],"result":{}}))
            .unwrap();
        let methods = acknowledged
            .writes
            .iter()
            .map(|frame| frame["method"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>();
        assert!(
            methods.contains(&"thread/read".to_owned()),
            "the compacted thread's state is never asked for: {methods:?}"
        );
        assert!(
            methods.contains(&"thread/turns/list".to_owned()),
            "the rewritten history is never asked for: {methods:?}"
        );
    }

    /// A plan reaches the surface as the steps it is made of. It used to
    /// arrive as a transcript message holding the raw JSON of the payload,
    /// which is the same information in the one shape nobody can read.
    #[test]
    fn a_published_plan_arrives_as_its_steps() {
        let mut adapter = adapter();
        let step = adapter
            .receive(json!({"method":"turn/plan/updated","params":{"threadId":"thread-exact","turnId":"turn","plan":{
                "explanation":"why",
                "plan":[
                    {"step":"Read the view","status":"completed"},
                    {"step":"Split the status line","status":"in_progress"},
                    {"step":"   ","status":"pending"},
                    {"step":"Pin the plan","status":"pending"}]}}}))
            .unwrap();
        let steps = step
            .events
            .iter()
            .find_map(|event| match event {
                Event::Plan { steps } => Some(steps.clone()),
                _ => None,
            })
            .expect("the plan reaches the surface");
        assert_eq!(
            steps,
            vec![
                PlanStep {
                    text: "Read the view".into(),
                    status: PlanState::Done
                },
                PlanStep {
                    text: "Split the status line".into(),
                    status: PlanState::Active
                },
                PlanStep {
                    text: "Pin the plan".into(),
                    status: PlanState::Pending
                },
            ],
            "a step with no text of its own would pin a blank row"
        );
        assert!(
            !step
                .events
                .iter()
                .any(|event| matches!(event, Event::Item(_))),
            "the raw payload still reaches the transcript"
        );
    }
}
