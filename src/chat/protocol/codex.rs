//! Codex app-server JSON-RPC, verified against installed 0.159.3 schemas.
//! Wire contracts: `codex app-server generate-json-schema --experimental`,
//! https://github.com/openai/codex/tree/rust-v0.153.4/codex-rs/app-server-protocol
//! Pending requests are replayed to joining connections with the original ID;
//! the first answer wins (app-server/src/outgoing_message.rs). Never auto-answer.
use super::{Adapter, ProcessSpec, ServerIdentity, Step, TransportSpec};
use crate::account::{self, LiveReport};
use crate::chat::{
    self, Answer, Attachment, Capabilities, ChangeKind, Choice, Command, CommandAction, Event,
    Field, FileChange, Item, ItemContent, Launch, Model, Phase, PlanState, PlanStep, PromptMode,
    Request, Role, SearchResult, Skill, SkillReach, ToolStatus, Usage,
};
use serde_json::{Value, json};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::{collections::BTreeMap, path::PathBuf};

mod images;

/// The Codex configuration key for a thread's reasoning effort. A launch
/// recipe names it as `--config model_reasoning_effort=<effort>`, the way the
/// Codex CLI takes it, and a new thread receives it as a `thread/start`
/// configuration override.
pub const REASONING_EFFORT_KEY: &str = "model_reasoning_effort";

/// The shortest directory suffix that distinguishes one same-name skill from
/// its peers. A skill can declare a name different from its directory name.
fn skill_source<'a>(path: &str, others: impl Iterator<Item = &'a str>) -> String {
    fn directory_components(path: &str) -> Vec<String> {
        std::path::Path::new(path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new(path))
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect()
    }
    let directory = directory_components(path);
    let peers = others.map(directory_components).collect::<Vec<_>>();
    let length = (1..=directory.len())
        .find(|length| {
            peers.iter().all(|peer| {
                peer.len() < *length
                    || peer[peer.len() - length..] != directory[directory.len() - length..]
            })
        })
        .unwrap_or(directory.len());
    let suffix = &directory[directory.len().saturating_sub(length)..];
    // When all skill directory names match each other, the distinguishing
    // ancestor is enough for the visible label.
    let common_leaf = peers.iter().all(|peer| peer.last() == directory.last());
    if common_leaf && suffix.len() > 1 {
        suffix[..suffix.len() - 1].join("/")
    } else {
        suffix.join("/")
    }
}

const MAX_PENDING: usize = 128;
const MAX_MODELS: usize = 512;
const MAX_ACTIVE_ITEMS: usize = 128;
const MAX_ITEM_BYTES: usize = 512 * 1024;
/// Output keeps its first quarter and a rolling tail, with accounting kept
/// separately from provider text so marker-like output cannot forge omission.
#[derive(Default)]
struct OutputWindow {
    omitted: usize,
    head: usize,
}

fn append_display_output(
    output: &mut String,
    delta: &str,
    limit: usize,
    window: &mut OutputWindow,
) -> bool {
    if delta.is_empty() {
        return false;
    }
    if limit < 64 {
        // Large command identity fields can leave very little output budget.
        // Preserve the newest UTF-8-safe suffix even when no marker fits.
        if delta.len() >= limit {
            *output = delta[delta.ceil_char_boundary(delta.len().saturating_sub(limit))..].into();
        } else {
            let keep = limit - delta.len();
            let start = output.ceil_char_boundary(output.len().saturating_sub(keep));
            output.drain(..start);
            output.push_str(delta);
        }
        window.omitted = 0;
        window.head = 0;
        return true;
    }
    if window.omitted == 0 && output.len().saturating_add(delta.len()) <= limit {
        output.push_str(delta);
        return true;
    }
    // Reserve enough space for the largest usize count and marker punctuation.
    let budget = limit.saturating_sub(64);
    let (head, tail) = if window.omitted == 0 {
        let mut whole = std::mem::take(output);
        whole.push_str(delta);
        let mut head = whole.floor_char_boundary((budget / 4).min(whole.len()));
        if let Some(line) = whole[..head].rfind('\n') {
            head = line + 1;
        }
        (whole[..head].to_owned(), whole[head..].to_owned())
    } else {
        let marker = format!("\n[... {} bytes omitted ...]\n", window.omitted);
        let mut tail = output[window.head + marker.len()..].to_owned();
        tail.push_str(delta);
        (output[..window.head].to_owned(), tail)
    };
    let tail_budget = budget.saturating_sub(head.len());
    let mut start = tail.ceil_char_boundary(tail.len().saturating_sub(tail_budget));
    // A rolling tail begins on a complete line where possible. For long
    // single-line output preserve a UTF-8-safe suffix instead.
    if start > 0
        && let Some(line) = tail[start..].find('\n')
    {
        if start + line + 1 < tail.len() {
            start += line + 1;
        }
    }
    window.omitted = window.omitted.saturating_add(start);
    window.head = head.len();
    *output = head;
    let marker = format!("\n[... {} bytes omitted ...]\n", window.omitted);
    if marker.len() <= limit.saturating_sub(output.len()) {
        output.push_str(&marker);
        output.push_str(&tail[start..]);
    } else {
        output.truncate(output.floor_char_boundary(limit));
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
    create_inner(launch, None, None)
}

pub(crate) fn create_with_listener(
    launch: &Launch,
    socket: PathBuf,
) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    create_inner(launch, Some(socket), None)
}

/// Only authenticated holder recovery supplies this identity. The runtime
/// additionally verifies the exact retained app-server peer before initialize.
pub(crate) fn create_restored(
    recovery: &crate::chat::native_holder::Recovery,
) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    let confirmed = recovery
        .server_identity
        .clone()
        .ok_or("Missing retained server identity")?;
    if recovery.server_process.is_none() {
        return Err("Missing retained app-server process identity".into());
    }
    create_inner(&recovery.source.launch, None, Some(confirmed))
}

fn create_inner(
    launch: &Launch,
    owned_socket: Option<PathBuf>,
    retained_identity: Option<ServerIdentity>,
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
            "--config" | "-c" if launch.session_id.is_none() => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("Missing value for Codex {arg}"))?;
                // Only an override with a typed `thread/start` equivalent is
                // translated; any other key would silently change meaning.
                let effort = value
                    .strip_prefix(REASONING_EFFORT_KEY)
                    .and_then(|rest| rest.strip_prefix('='))
                    .map(|effort| {
                        effort
                            .strip_prefix('"')
                            .and_then(|effort| effort.strip_suffix('"'))
                            .unwrap_or(effort)
                    })
                    .filter(|effort| crate::agent_execution::valid_reasoning_effort(effort))
                    .ok_or_else(|| {
                        format!(
                            "Codex launch option {arg} {value:?} cannot safely be translated to native app-server mode"
                        )
                    })?;
                start["config"][REASONING_EFFORT_KEY] = json!(effort);
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
    let expected_home = if let Some(identity) = &retained_identity {
        identity.canonical_home.clone()
    } else {
        std::fs::canonicalize(&home).map_err(|e| {
            format!(
                "Cannot verify Codex account directory {}: {e}",
                home.display()
            )
        })?
    };
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
            server_identity: None,
            retained_identity,
            thread: launch.session_id.clone(),
            start,
            alias,
            cwd: launch.recipe.working_directory.clone(),
            expected_home,
            next_id: 0,
            pending: BTreeMap::new(),
            requests: BTreeMap::new(),
            active_items: BTreeMap::new(),
            output_streams: BTreeMap::new(),
            output_windows: BTreeMap::new(),
            models: Vec::new(),
            listing: Vec::new(),
            modalities: BTreeMap::new(),
            selected: None,
            reasoning: None,
            selection_dirty: false,
            service_tier: None,
            service_tier_revision: 0,
            fast_models: Default::default(),
            effective_model: None,
            pricing: Default::default(),
            turn_pricing: Default::default(),
            turn: None,
            busy: false,
            lifecycle_revision: 0,
            suggestion_turn: None,
            prediction_turn: None,
            predictions_unsupported: false,
            subagent_status: None,
            subagent_waiting: false,
            subagent_detail: String::new(),
            ready: false,
            attached: false,
            direct_input: false,
            cursor: None,
            history_initialized: false,
            history_loading: false,
            history_refresh: false,
            queue_available: true,
            queue_stale: false,
            queue_listing: Vec::new(),
            queue_cursors: std::collections::BTreeSet::new(),
            skills_stale: false,
            skill_paths: Vec::new(),
            materialized: launch.session_id.is_some(),
            execution: None,
            children: BTreeMap::new(),
            branch_points: Vec::new(),
            is_child: false,
            child_buffer: BTreeMap::new(),
        }),
    ))
}

#[derive(Clone)]
enum Pending {
    LoginRecovery {
        failure: String,
        expected: Option<crate::account::Identity>,
        revision: u64,
    },
    Initialize,
    Attach,
    Models,
    Skills,
    History {
        earlier: bool,
    },
    Turn {
        revision: u64,
        model: Option<String>,
        tier: Option<String>,
    },
    /// Compaction rewrites the thread's history and its context occupancy, so
    /// its acknowledgement is what a re-read hangs off: nothing else on the
    /// wire announces that the figures a reader was looking at have moved.
    Compact,
    Other,
    FastMode {
        enabled: bool,
        revision: u64,
        model: Option<String>,
    },
    QueueAdd,
    /// Removing the named entry, whose answer says whether it left the queue
    /// undelivered.
    QueueDelete(String),
    QueueList,
    ChildResume(String),
    ChildRpc {
        child: String,
        original_id: Value,
    },
    ChildList,
    /// Interrupting one child's turn, which leaves the parent and the other
    /// children running.
    StopChild,
    Prediction,
    Fork,
}

fn permission_mode(approval: &Value, sandbox: &Value) -> Option<&'static str> {
    match (approval.as_str(), sandbox["type"].as_str()) {
        (Some("never"), Some("dangerFullAccess")) => Some("yolo"),
        (Some("on-request"), Some("workspaceWrite")) => Some("workspaceWrite"),
        (Some("on-request"), Some("readOnly")) => Some("readOnly"),
        _ => None,
    }
}
struct PendingRequest {
    frame: Value,
    actions: BTreeMap<String, Value>,
    fields: Vec<Field>,
    kind: RequestKind,
}
fn execution_end(run: crate::model::RunId, outcome: crate::notifications::Kind, step: &mut Step) {
    use crate::agent_execution::ProviderEvent;
    for event in [
        ProviderEvent::Scope,
        ProviderEvent::Settled,
        ProviderEvent::Finished { outcome },
    ] {
        step.events.push(Event::Execution { run, event });
    }
}

enum RequestKind {
    Decision,
    Permissions,
    Questions,
    Elicitation,
}
#[derive(Default)]
struct Codex {
    server_identity: Option<ServerIdentity>,
    retained_identity: Option<ServerIdentity>,
    thread: Option<String>,
    start: Value,
    alias: Option<PathBuf>,
    cwd: PathBuf,
    expected_home: PathBuf,
    next_id: u64,
    pending: BTreeMap<String, Pending>,
    requests: BTreeMap<String, PendingRequest>,
    active_items: BTreeMap<String, Item>,
    /// What is held of each running command's output between deltas: the
    /// start of an escape sequence the next delta finishes. An entry exists
    /// only for an item in `active_items`, and goes when the item does.
    output_streams: BTreeMap<String, super::terminal_output::Stream>,
    output_windows: BTreeMap<String, OutputWindow>,
    models: Vec<Model>,
    /// The pages of a model listing still under way. The listing is
    /// reported whole, since a model missing from part of it reads as one
    /// Codex does not offer.
    listing: Vec<Model>,
    modalities: BTreeMap<String, Vec<String>>,
    selected: Option<String>,
    reasoning: Option<String>,
    selection_dirty: bool,
    service_tier: Option<String>,
    service_tier_revision: u64,
    fast_models: std::collections::BTreeSet<String>,
    effective_model: Option<String>,
    pricing: chat::pricing::CostTracker,
    turn_pricing: std::collections::VecDeque<(String, Option<String>, Option<String>)>,
    turn: Option<String>,
    busy: bool,
    lifecycle_revision: u64,
    suggestion_turn: Option<String>,
    prediction_turn: Option<String>,
    predictions_unsupported: bool,
    subagent_status: Option<ToolStatus>,
    subagent_waiting: bool,
    subagent_detail: String,
    ready: bool,
    attached: bool,
    direct_input: bool,
    cursor: Option<String>,
    history_initialized: bool,
    history_loading: bool,
    history_refresh: bool,
    queue_available: bool,
    queue_stale: bool,
    queue_listing: Vec<chat::QueuedMessage>,
    queue_cursors: std::collections::BTreeSet<String>,
    /// Whether the catalog was invalidated again while a request for it was
    /// still outstanding, so exactly one refetch follows that answer.
    skills_stale: bool,
    /// Each skill in the catalog by name and the path to its `SKILL.md`,
    /// which is how a message's input names one exactly.
    skill_paths: Vec<(String, String)>,
    materialized: bool,
    execution: Option<Execution>,
    children: BTreeMap<String, Box<Codex>>,
    branch_points: Vec<chat::BranchPoint>,
    is_child: bool,
    child_buffer: BTreeMap<String, Vec<Value>>,
}

struct Execution {
    run: crate::model::RunId,
    request: String,
    turn: Option<String>,
    /// A shared server may deliver completion before its turn/start response.
    /// Do not guess which observed turn belongs to this accepted request.
    early: BTreeMap<String, Value>,
    early_bytes: usize,
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
    fn discover_child(
        &mut self,
        id: &str,
        metadata: &Value,
        step: &mut Step,
    ) -> Result<(), String> {
        if self.is_child || id.is_empty() || id.len() > 256 || self.thread.as_deref() == Some(id) {
            return Ok(());
        }
        let discovered = !self.children.contains_key(id);
        if discovered {
            if self.children.len() >= 32 {
                let completed = self
                    .children
                    .iter()
                    .find(|(child_id, child)| {
                        child.ready
                            && !child.busy
                            && !self.child_buffer.contains_key(*child_id)
                            && !self.requests.values().any(|request| {
                                request.frame["params"]["threadId"].as_str()
                                    == Some(child_id.as_str())
                            })
                    })
                    .map(|(id, _)| id.clone());
                if let Some(completed) = completed {
                    self.children.remove(&completed);
                    self.call(
                        step,
                        "thread/unsubscribe",
                        json!({"threadId":completed}),
                        Pending::Other,
                    )?;
                }
            }
            if self.children.len() >= 32 || self.pending.len() >= MAX_PENDING {
                return Ok(());
            }
            let mut child = Codex::default();
            child.thread = Some(id.into());
            child.cwd = self.cwd.clone();
            child.expected_home = self.expected_home.clone();
            child.is_child = true;
            self.children.insert(id.into(), Box::new(child));
            self.child_buffer.insert(id.into(), Vec::new());
            self.call(
                step,
                "thread/resume",
                json!({"threadId":id,"excludeTurns":true,"initialTurnsPage":{"limit":4,"sortDirection":"desc","itemsView":"full"}}),
                Pending::ChildResume(id.into()),
            )?;
        }
        let model = metadata["model"]
            .as_str()
            .filter(|model| !model.is_empty())
            .map(str::to_owned);
        if let Some(model) = &model
            && let Some(child) = self.children.get_mut(id)
        {
            child.selected = Some(model.clone());
        }
        let status = agent_status(&metadata["status"]);
        if let Some(status) = status
            && let Some(child) = self.children.get_mut(id)
        {
            // Sparse collab metadata must not reset a known terminal outcome.
            child.subagent_status = Some(status);
        }
        step.events.push(Event::Subagent {
            model,
            waiting: None,
            detail: None,
            id: id.into(),
            kind: metadata["agentNickname"]
                .as_str()
                .or_else(|| metadata["agentRole"].as_str())
                .unwrap_or(if discovered { "Codex agent" } else { "" })
                .into(),
            description: metadata["message"]
                .as_str()
                .or_else(|| metadata["preview"].as_str())
                .unwrap_or_default()
                .into(),
            status,
            // A child is a thread of its own, not work inside the parent's turn.
            background: Some(true),
        });
        Ok(())
    }
    // Child parsers share a server connection, so their local RPC IDs must
    // never escape into the parent's ID space. Keep the original correlation
    // inside the child and allocate every wire request through the parent.
    fn child_step(&mut self, id: &str, content: Step, step: &mut Step) -> Result<(), String> {
        if content.events.iter().any(|event| {
            matches!(
                event,
                Event::Status { .. }
                    | Event::Waiting(_)
                    | Event::Models { .. }
                    | Event::TurnFinished { .. }
                    | Event::Ready(false)
            )
        }) {
            let child = self.children.get(id);
            step.events.push(Event::Subagent {
                model: child.and_then(|child| child.selected.clone()),
                waiting: child.map(|child| child.subagent_waiting),
                detail: child.map(|child| child.subagent_detail.clone()),
                id: id.into(),
                kind: String::new(),
                description: String::new(),
                status: self
                    .children
                    .get(id)
                    .and_then(|child| child.subagent_status),
                background: None,
            });
        }
        step.events.extend(chat::child_events(id, content.events));
        for frame in content.writes {
            if let (Some(method), Some(original_id)) = (frame["method"].as_str(), frame.get("id")) {
                self.call(
                    step,
                    method,
                    frame["params"].clone(),
                    Pending::ChildRpc {
                        child: id.into(),
                        original_id: original_id.clone(),
                    },
                )?;
            } else {
                step.writes.push(frame);
            }
        }
        Ok(())
    }
    fn finish_execution(&mut self, turn: &Value, step: &mut Step) -> Result<(), String> {
        let Some(execution) = self.execution.as_mut() else {
            return Ok(());
        };
        let id = string(turn, "id")?;
        if execution.turn.is_none() {
            let bytes = serde_json::to_vec(turn)
                .map_err(|error| error.to_string())?
                .len();
            if execution.early.len() >= 8
                || execution.early_bytes.saturating_add(bytes)
                    > crate::agent_execution::MAX_RESULT_BYTES
            {
                return Err(
                    "Codex completed turns exceeded pending request correlation capacity".into(),
                );
            }
            execution.early_bytes += bytes;
            execution.early.insert(id.to_owned(), turn.clone());
            return Ok(());
        }
        if execution.turn.as_deref() != Some(id) {
            return Ok(());
        }
        let outcome = match turn["status"].as_str() {
            Some("completed") => crate::notifications::Kind::Succeeded,
            Some("failed") => crate::notifications::Kind::Failed,
            Some("interrupted") => crate::notifications::Kind::Cancelled,
            _ => return Err("Codex completed an accepted run with an unknown outcome".into()),
        };
        let execution = self.execution.take().unwrap();
        // Full completed-turn items also cover output that arrived before the
        // request acknowledgement. They upsert the same transcript identities.
        for value in array(turn, "items")? {
            step.events
                .extend(parse_item(value, true, id)?.into_iter().map(Event::Item));
        }
        if let Some(message) = turn["error"]["message"].as_str() {
            step.events.push(Event::Error {
                message: message.into(),
                fatal: false,
            });
        }
        execution_end(execution.run, outcome, step);
        Ok(())
    }

    /// The first queued prompt needs the catalog to resolve skill mentions
    /// into typed inputs. Attachment and catalog responses can arrive in
    /// either order; subsequent catalog refreshes leave the session usable.
    fn finish_setup(&mut self, step: &mut Step) {
        if self.attached
            && !self.ready
            && !self
                .pending
                .values()
                .any(|pending| matches!(pending, Pending::Skills))
        {
            self.ready = true;
            self.model_events(step);
            step.events.push(Event::Ready(true));
            if self.direct_input
                && let Err(message) = self.read_queue(step)
            {
                step.events.push(Event::Error {
                    message,
                    fatal: false,
                });
            }
        }
    }

    fn read_queue(&mut self, step: &mut Step) -> Result<(), String> {
        if !self.queue_available {
            return Ok(());
        }
        if self
            .pending
            .values()
            .any(|pending| matches!(pending, Pending::QueueList))
        {
            self.queue_stale = true;
            return Ok(());
        }
        self.queue_listing.clear();
        self.queue_cursors.clear();
        self.queue_stale = false;
        self.call(
            step,
            "thread/queue/list",
            json!({"threadId":self.thread, "limit":64}),
            Pending::QueueList,
        )
    }

    fn queue_page(&mut self, result: &Value, step: &mut Step) -> Result<(), String> {
        for entry in array(result, "data")? {
            if self.queue_listing.len() >= chat::QueuedMessage::LIMIT {
                break;
            }
            let id = string(entry, "id")?.to_owned();
            let inputs = array(entry, "input")?;
            let mut parts = Vec::new();
            let mut attachments = Vec::new();
            for input in inputs {
                parts.push(match input["type"].as_str() {
                    Some("text") => string(input, "text")?.to_owned(),
                    Some("localImage") => {
                        attachments.push(chat::Attachment {
                            path: string(input, "path")?.into(),
                        });
                        continue;
                    }
                    Some("image") => "Image attachment".to_owned(),
                    // A skill is named in the text it was queued with.
                    Some("skill") if inputs.iter().any(|input| input["type"] == "text") => continue,
                    Some("skill" | "mention") => format!(
                        "{}: {}",
                        input["type"].as_str().unwrap(),
                        input["name"].as_str().unwrap_or_default()
                    ),
                    _ => "Attachment".to_owned(),
                });
            }
            let mut message = chat::QueuedMessage::new(id, parts.join("\n"));
            // Only what the composer can hold again counts as the whole
            // message: one text, the skills that text names again when it is
            // sent, and local images.
            let texts = inputs
                .iter()
                .filter(|input| input["type"] == "text")
                .collect::<Vec<_>>();
            let named = texts
                .first()
                .and_then(|input| input["text"].as_str())
                .map(|text| self.named_skills(text))
                .unwrap_or_default();
            message.editable &= texts.len() <= 1
                && inputs.iter().all(|input| match input["type"].as_str() {
                    Some("text" | "localImage") => true,
                    Some("skill") => named.iter().any(|(name, path)| {
                        input["name"].as_str() == Some(name) && input["path"].as_str() == Some(path)
                    }),
                    _ => false,
                });
            message.attachments = attachments;
            self.queue_listing.push(message);
        }
        if let Some(cursor) = result["nextCursor"].as_str() {
            if cursor.len() > 4096
                || !self.queue_cursors.insert(cursor.to_owned())
                || self.queue_cursors.len() > chat::QueuedMessage::LIMIT
            {
                return Err("Codex returned an invalid queue pagination cursor".into());
            }
            self.call(
                step,
                "thread/queue/list",
                json!({"threadId":self.thread, "limit":64, "cursor":cursor}),
                Pending::QueueList,
            )?;
        } else if self.queue_stale {
            self.read_queue(step)?;
        } else {
            step.events
                .push(Event::QueueChanged(std::mem::take(&mut self.queue_listing)));
        }
        Ok(())
    }

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
            queue: self.ready && self.direct_input && self.queue_available,
            attachments: self.direct_input
                && self
                    .selected
                    .as_ref()
                    .and_then(|id| self.modalities.get(id))
                    .is_some_and(|m| m.iter().any(|m| m == "image")),
            compact: self.ready && self.direct_input,
            model_selection: self.direct_input && !self.models.is_empty(),
            fast_mode: self.ready
                && self.direct_input
                && (self.service_tier.as_deref() == Some("fast")
                    || self
                        .selected
                        .as_ref()
                        .is_some_and(|model| self.fast_models.contains(model))),
            reasoning_selection: self.direct_input
                && model.is_some_and(|m| !m.reasoning.is_empty()),
            permission_modes: if self.direct_input {
                vec!["readOnly".into(), "workspaceWrite".into(), "yolo".into()]
            } else {
                Vec::new()
            },
            skill_reach: SkillReach::Anywhere,
            branch: self.direct_input,
            queue_remove: self.queue_available && self.direct_input,
            queue_start: self.queue_available && self.direct_input,
            stop_subagent: self.ready,
            ..Capabilities::default()
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
        self.fast_mode_event(step);
    }
    fn fast_mode_event(&self, step: &mut Step) {
        step.events.push(Event::FastMode(chat::FastMode {
            state: if self.service_tier.as_deref() == Some("fast") {
                chat::FastModeState::On
            } else {
                chat::FastModeState::Off
            },
            disabled_reason: None,
        }));
    }
    fn remember_turn_pricing(&mut self, id: &str, model: Option<String>, tier: Option<String>) {
        if self.turn_pricing.iter().any(|(turn, _, _)| turn == id) {
            return;
        }
        if self.turn_pricing.len() >= 32 {
            self.turn_pricing.pop_front();
        }
        self.turn_pricing.push_back((id.into(), model, tier));
    }
    fn history(&mut self, step: &mut Step, cursor: Option<String>) -> Result<(), String> {
        if !self.materialized {
            return Ok(());
        }
        if self.history_loading {
            return Err("Codex history is already loading".into());
        }
        let earlier = cursor.is_some();
        self.call(step, "thread/turns/list", json!({"threadId":self.thread,"cursor":cursor,"limit":20,"sortDirection":"desc","itemsView":"full"}), Pending::History { earlier })?;
        self.history_loading = true;
        step.events.push(if earlier {
            Event::HistoryLoading(true)
        } else {
            Event::LatestHistoryStarted
        });
        Ok(())
    }
    /// Re-reads the thread's metadata and its history.
    ///
    /// An outstanding page finishes first, followed by one newest-page reread.
    /// Repeated refreshes coalesce instead of issuing competing requests. This
    /// also lets a compaction acknowledgement refresh history without failing
    /// merely because the reader was loading an older page.
    fn reread(&mut self, step: &mut Step) -> Result<(), String> {
        if self.history_loading {
            self.history_refresh = true;
            return Ok(());
        }
        self.call(
            step,
            "thread/read",
            json!({"threadId":self.thread,"includeTurns":false}),
            Pending::Other,
        )?;
        self.history(step, None)
    }
    fn finish_history(&mut self, step: &mut Step) -> Result<(), String> {
        self.history_loading = false;
        if std::mem::take(&mut self.history_refresh) {
            self.reread(step)
        } else {
            step.events.push(Event::HistoryLoading(false));
            Ok(())
        }
    }
    /// Whether to hold `item`, parsed from `value`, for the deltas Codex may
    /// still write into it. Past the bound a running item goes unheld instead
    /// of failing the session: it loses only its streamed output, which its
    /// completion then supplies whole.
    fn tracks(&self, value: &Value, item: &Item) -> bool {
        streams_updates(value, &item.content)
            && (self.active_items.len() < MAX_ACTIVE_ITEMS
                || self.active_items.contains_key(&item.id))
    }
    /// An idle thread says nothing about whether its last turn succeeded.
    /// Recover that outcome from the latest page, never an older page or while
    /// live work is running. Buffered live notifications are replayed afterward.
    fn child_history_outcome(&mut self, turn: Option<&Value>, step: &mut Step) {
        if !self.is_child || self.busy {
            return;
        }
        let Some(status) = turn.and_then(|turn| agent_status(&turn["status"])) else {
            return;
        };
        if status == ToolStatus::Running || self.subagent_status == Some(status) {
            return;
        }
        self.subagent_status = Some(status);
        self.subagent_waiting = false;
        self.subagent_detail = turn
            .map(|turn| turn_detail(&turn["status"]))
            .unwrap_or_default();
        step.events.push(Event::Status {
            busy: false,
            detail: turn
                .map(|turn| turn_detail(&turn["status"]))
                .unwrap_or_default(),
        });
    }

    fn page(&mut self, page: &Value, step: &mut Step, earlier: bool) -> Result<(), String> {
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
                if !active && value["type"] == "userMessage" {
                    if let Some(label) = value["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find_map(|input| input["text"].as_str())
                        .and_then(crate::account::clean_label)
                    {
                        if !self.branch_points.iter().any(|point| point.id == turn_id)
                            && self.branch_points.len() < 128
                        {
                            self.branch_points.push(chat::BranchPoint {
                                id: turn_id.into(),
                                label,
                            });
                        }
                    }
                }
                if value["type"] == "collabAgentToolCall" {
                    for id in value["receiverThreadIds"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .take(32)
                    {
                        let mut metadata = value["agentsStates"][id].clone();
                        // History describes the child at the time of this call.
                        // A live subscription or prior hydration owns its current
                        // outcome; paging an old spawn must not restart it.
                        if self.children.contains_key(id)
                            && let Some(metadata) = metadata.as_object_mut()
                        {
                            metadata.remove("status");
                            metadata.remove("model");
                        }
                        self.discover_child(id, &metadata, step)?;
                    }
                }
                for item in parse_item(value, !active, turn_id)? {
                    if active && self.busy {
                        if self.tracks(value, &item) {
                            self.active_items
                                .entry(item.id.clone())
                                .or_insert_with(|| item.clone());
                        } else {
                            // The page is newer than a completion this
                            // adapter missed while it was not reading.
                            self.active_items.remove(&item.id);
                            self.output_streams.remove(&item.id);
                            self.output_windows.remove(&item.id);
                        }
                    }
                    items.push(item);
                }
            }
        }
        if !earlier {
            self.child_history_outcome(array(page, "data")?.first(), step);
        }
        // Refreshing the live end must not rewind the older-history traversal
        // through pages already cached. Commit only after validating the page;
        // a failed decode must preserve the cursor the reader can retry.
        if earlier || !self.history_initialized {
            self.cursor = page["nextCursor"].as_str().map(str::to_owned);
            self.history_initialized = true;
        }
        step.events.push(if earlier {
            Event::History {
                items,
                has_earlier: self.cursor.is_some(),
            }
        } else {
            Event::LatestHistory {
                items,
                has_earlier: self.cursor.is_some(),
            }
        });
        step.events
            .push(Event::BranchPoints(self.branch_points.clone()));
        Ok(())
    }
    fn input(&self, text: String, attachments: Vec<Attachment>) -> Result<Vec<Value>, String> {
        let mut input = Vec::with_capacity(1 + attachments.len());
        // Each skill the text names goes with it as an input of its own, as
        // Codex's own composer sends one: the text names a skill only by its
        // name, which loads nothing when two skills share it.
        let skills = self.named_skills(&text);
        if !text.is_empty() {
            input.push(json!({"type":"text","text":text}));
        }
        input.extend(
            skills
                .into_iter()
                .map(|(name, path)| json!({"type":"skill","name":name,"path":path})),
        );
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
    /// The catalog's skills `text` names, each once, the way Codex reads
    /// them: a `[$name](path)` link names the skill at that path, and a bare
    /// `$name` the one skill of that name, if only one has it.
    fn named_skills(&self, text: &str) -> Vec<(&str, &str)> {
        let mut named: Vec<(&str, &str)> = Vec::new();
        for mention in skill_mentions(text) {
            let skill = match mention {
                Mention::Linked(path) => self
                    .skill_paths
                    .iter()
                    .find(|(_, known)| known == path)
                    .or_else(|| {
                        let resolved = std::fs::canonicalize(path).ok()?;
                        self.skill_paths.iter().find(|(_, known)| {
                            std::fs::canonicalize(known).is_ok_and(|known| known == resolved)
                        })
                    }),
                Mention::Named(name) => {
                    let mut matching = self.skill_paths.iter().filter(|(known, _)| known == name);
                    match (matching.next(), matching.next()) {
                        (Some(skill), None) => Some(skill),
                        _ => None,
                    }
                }
            };
            if let Some((name, path)) = skill
                && !named.iter().any(|(_, known)| known == path)
            {
                named.push((name, path));
            }
        }
        named
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
        if let Some(child_id) = params["threadId"].as_str()
            && let Some(child) = self.children.get_mut(child_id)
        {
            child.request(frame.clone(), step)?;
            self.requests.append(&mut child.requests);
            for event in &mut step.events {
                if let Event::Request(request) = event {
                    request.title = format!("Child agent · {}", request.title);
                }
            }
            return Ok(());
        }
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
            kind: match &kind {
                RequestKind::Decision | RequestKind::Permissions => chat::RequestKind::Approval,
                RequestKind::Questions => chat::RequestKind::Question,
                RequestKind::Elicitation => chat::RequestKind::Consent,
            },
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
    fn server_identity(&self) -> Option<&ServerIdentity> {
        self.server_identity.as_ref()
    }
    fn command_for_run(
        &mut self,
        run: crate::model::RunId,
        command: Command,
    ) -> Result<Step, String> {
        if self.execution.is_some()
            || !matches!(
                &command,
                Command::Prompt {
                    mode: PromptMode::Send,
                    ..
                }
            )
        {
            return Err("A correlated Codex run requires one idle direct prompt".into());
        }
        let mut step = self.command(command)?;
        let request = step
            .writes
            .iter()
            .find(|frame| frame["method"] == "turn/start")
            .and_then(|frame| frame["id"].as_str())
            .ok_or("Codex did not create the prompt request")?
            .to_owned();
        self.execution = Some(Execution {
            run,
            request: request.clone(),
            turn: None,
            early: BTreeMap::new(),
            early_bytes: 0,
        });
        step.events.insert(
            0,
            Event::Execution {
                run,
                event: crate::agent_execution::ProviderEvent::Started {
                    key: format!("codex-request:{request}"),
                },
            },
        );
        Ok(step)
    }
    fn initialize(&mut self) -> Result<Step, String> {
        let mut step = Step::default();
        self.ready = false;
        self.attached = false;
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
                        if self.busy || self.pending.values().any(|p| matches!(p, Pending::Turn { .. })) { return Err("A Codex turn is active; explicitly steer or queue instead".into()); }
                        let mut params = json!({"threadId":self.thread,"input":input});
                        if self.selection_dirty { params["model"] = json!(self.selected); params["effort"] = json!(self.reasoning); }
                        self.call(&mut step, "turn/start", params, Pending::Turn { revision: self.lifecycle_revision, model: self.selected.clone(), tier: self.service_tier.clone() })?;
                        self.suggestion_turn = None;
                        self.prediction_turn = None;
                        step.events.push(Event::PromptSuggestionCleared);
                    }
                    PromptMode::Steer => {
                        let turn = self.turn.as_ref().ok_or("There is no known active Codex turn to steer")?;
                        self.call(&mut step, "turn/steer", json!({"threadId":self.thread,"expectedTurnId":turn,"input":input}), Pending::Other)?;
                    }
                    PromptMode::Queue => {
                        self.call(&mut step, "thread/queue/add", json!({"threadId":self.thread,"clientUserMessageId":uuid::Uuid::new_v4().to_string(),"input":input}), Pending::QueueAdd)?;
                        step.events.push(Event::Status { busy: self.busy, detail: "Submitting to Codex's native queue".into() });
                    }
                }
            }
            Command::Interrupt => {
                let turn = self.turn.as_ref().ok_or("There is no known active Codex turn to interrupt")?;
                self.call(&mut step, "turn/interrupt", json!({"threadId":self.thread,"turnId":turn}), Pending::Other)?;
            }
            // The client's form of the model's own `interrupt_agent`: a child
            // is a thread of its own, so interrupting its turn reaches neither
            // the parent nor its siblings. Unlike input, this is not refused
            // for a child the server keeps from direct use.
            Command::StopSubagent { id } => {
                let child = self.children.get(&id).ok_or("This Codex subagent is no longer followed")?;
                let turn = child.turn.clone().ok_or("This Codex subagent has no active turn to stop")?;
                self.call(&mut step, "turn/interrupt", json!({"threadId":id,"turnId":turn}), Pending::StopChild)?;
            }
            operation @ (Command::RemoveQueued { .. } | Command::StartQueued { .. }) => {
                if !self.queue_available || !self.direct_input { return Err("Codex queue controls are unavailable".into()); }
                let (method, id, pending) = match operation {
                    Command::RemoveQueued { id } => ("thread/queue/delete", id.clone(), Pending::QueueDelete(id)),
                    Command::StartQueued { id } => ("thread/queue/start", id, Pending::QueueAdd),
                    _ => unreachable!(),
                };
                self.call(&mut step, method, json!({"threadId":self.thread,"queuedSubmissionId":id}), pending)?;
            }
            Command::SetInterruptMode { .. } => return Err("Codex uses explicit steer and queue operations".into()),
            Command::ListBranches => self.history(&mut step, None)?,
            Command::Branch { id } => {
                if self.busy || !self.direct_input { return Err("Fork requires an idle writable Codex conversation".into()); }
                if id.as_ref().is_some_and(|id| !self.branch_points.iter().any(|point| &point.id == id)) { return Err("Choose a completed turn from this conversation".into()); }
                self.call(&mut step, "thread/fork", json!({"threadId":self.thread,"lastTurnId":id,"excludeTurns":true}), Pending::Fork)?;
            }
            Command::Answer { request_id, action, answers } => self.answer(request_id, action, answers, &mut step)?,
            Command::SetModel { model, reasoning } => {
                if !self.direct_input { return Err("This Codex thread does not accept direct input".into()); }
                let selected = self.models.iter().find(|m| m.id == model).ok_or("Choose a model from the Codex model catalog")?;
                if (self.service_tier.as_deref() == Some("fast") || self.pending.values().any(|pending| matches!(pending, Pending::FastMode { enabled:true, .. }))) && !self.fast_models.contains(&model) { return Err("Turn off fast mode before selecting a Codex model that does not support it".into()); }
                if reasoning.as_ref().is_some_and(|r| !selected.reasoning.contains(r)) { return Err("The model does not advertise this reasoning effort".into()); }
                self.selected = Some(model); self.reasoning = reasoning; self.selection_dirty = true;
                self.model_events(&mut step);
                step.events.push(Event::Status { busy: self.busy, detail: "Model/reasoning selection will apply to your next new turn, not the running turn".into() });
            }
            Command::SetFastMode { enabled } => {
                if !self.capabilities().fast_mode {
                    return Err("The selected Codex model does not advertise fast mode".into());
                }
                if self.pending.values().any(|pending| matches!(pending, Pending::FastMode { .. })) {
                    return Err("Codex fast mode change is already pending".into());
                }
                let mut params = json!({"threadId":self.thread,"serviceTier":if enabled {Some("fast")} else {None}});
                // Model selection is normally staged until the next prompt.
                // Apply that same choice with its tier so the server never
                // validates fast against the previous model instead.
                if self.selection_dirty {
                    params["model"] = json!(self.selected);
                    params["effort"] = json!(self.reasoning);
                }
                self.call(&mut step, "thread/settings/update", params, Pending::FastMode {
                    enabled, revision: self.service_tier_revision,
                    model: self.selection_dirty.then(|| self.selected.clone()).flatten(),
                })?;
            }
            Command::SetPermissionMode { mode } => {
                if !self.direct_input || self.busy {
                    return Err("Codex permissions can be changed only on an idle, writable thread".into());
                }
                let (approval, sandbox) = match mode.as_str() {
                    "yolo" => ("never", "dangerFullAccess"),
                    "workspaceWrite" => ("on-request", "workspaceWrite"),
                    "readOnly" => ("on-request", "readOnly"),
                    _ => return Err("Unsupported Codex permission mode".into()),
                };
                self.call(&mut step, "thread/settings/update", json!({
                    "threadId": self.thread,
                    "approvalPolicy": approval,
                    "sandboxPolicy": {"type": sandbox},
                }), Pending::Other)?;
            }
            Command::Compact => {
                if !self.capabilities().compact || self.busy { return Err("Codex compaction requires an idle, writable thread".into()); }
                self.call(&mut step, "thread/compact/start", json!({"threadId":self.thread}), Pending::Compact)?;
            }
            Command::ResumeAfterLogin { failure, expected } => {
                if self.busy || self.pending.values().any(|pending| matches!(pending, Pending::Turn { .. } | Pending::LoginRecovery { .. })) {
                    return Err("Finish the active turn or login verification before resuming".into());
                }
                if !chat::requires_sign_in(crate::agent::Provider::Codex, &failure) {
                    return Err("This conversation has no recognized sign-in failure".into());
                }
                self.call(&mut step, "account/read", json!({"refreshToken":true}), Pending::LoginRecovery {failure, expected, revision: self.lifecycle_revision})?;
            }
            Command::Refresh => {
                if self.pending.len() > MAX_PENDING - 2 { return Err("Too many outstanding Codex requests".into()); }
                // Returning to the cached live end cancels the store's older
                // page pin. An already outstanding history read may populate
                // the cache without moving the reader back into that page.
                self.reread(&mut step)?;
            }
            Command::LoadFileChanges { .. } | Command::LoadFileChangePatches { .. } =>
                return Err("File changes are served by the transcript cache".into()),
            Command::LoadLater => return Err("Newer history is served by the transcript cache".into()),
            Command::LoadEarlier => {
                let cursor = self.cursor.clone().ok_or("No earlier Codex history is available")?;
                self.history(&mut step, Some(cursor))?;
            }
            Command::Slash { .. } => return Err("Codex app-server has no TUI slash-command dispatcher; use the native model, compact, interrupt and prompt controls".into()),
            Command::DismissPermissionDenial { tool_use_id } => step.events.push(Event::PermissionDenialDismissed { tool_use_id }),
            Command::RefreshPermissions | Command::UpdatePermissionSettings { .. } => return Err("Permission rule settings are available for Claude sessions".into()),
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
            if let Pending::ChildRpc {
                child: id,
                original_id,
            } = &pending
            {
                let mut response = frame;
                response["id"] = original_id.clone();
                if let Some(child) = self.children.get_mut(id) {
                    let content = child.receive(response)?;
                    self.child_step(id, content, &mut step)?;
                }
                if self
                    .children
                    .get(id)
                    .is_some_and(|child| !child.history_loading)
                {
                    for buffered in self.child_buffer.remove(id).unwrap_or_default() {
                        let content = self.children.get_mut(id).unwrap().receive(buffered)?;
                        self.child_step(id, content, &mut step)?;
                    }
                }
                return Ok(step);
            }
            if matches!(pending, Pending::History { .. }) {
                self.history_loading = false;
            }
            if let Some(error) = frame.get("error") {
                if let Pending::LoginRecovery {
                    failure, revision, ..
                } = &pending
                {
                    if *revision == self.lifecycle_revision && !self.busy {
                        step.events.push(Event::LoginVerificationFailed {
                            failure: failure.clone(),
                            detail: format!(
                                "Sign-in verification failed: {}",
                                error["message"]
                                    .as_str()
                                    .unwrap_or("Codex account/read failed")
                            ),
                        });
                    }
                    return Ok(step);
                }
                if matches!(pending, Pending::History { .. }) {
                    self.finish_history(&mut step)?;
                }
                if let Pending::ChildResume(id) = &pending {
                    let message = error["message"]
                        .as_str()
                        .unwrap_or("Unknown provider error");
                    step.events.extend(chat::child_events(
                        id,
                        vec![session_notice(format!(
                            "Child history unavailable: {message}. Live activity is still shown."
                        ))],
                    ));
                    // Hydration failed, not execution. Retain the live parser
                    // and replay the events observed while the read was pending.
                    if let Some(child) = self.children.get_mut(id) {
                        child.ready = true;
                    }
                    for frame in self.child_buffer.remove(id).unwrap_or_default() {
                        if let Some(child) = self.children.get_mut(id) {
                            let content = child.receive(frame)?;
                            self.child_step(id, content, &mut step)?;
                        }
                    }
                    return Ok(step);
                }
                if matches!(pending, Pending::Prediction) {
                    if error["code"].as_i64() == Some(-32601) {
                        self.predictions_unsupported = true;
                    }
                    // Predictions are optional. Failure cannot affect chat.
                    return Ok(step);
                }
                if matches!(pending, Pending::ChildList) {
                    return Ok(step);
                }
                if matches!(pending, Pending::QueueList) && error["code"].as_i64() == Some(-32601) {
                    self.queue_available = false;
                    self.queue_listing.clear();
                    step.events.push(Event::QueueChanged(Vec::new()));
                    step.events.push(Event::Capabilities(self.capabilities()));
                    return Ok(step);
                }
                if matches!(pending, Pending::Skills) {
                    // An app-server without a skills catalog answers with an
                    // error, and a composer that offers no completions is not
                    // worth putting a failure in the transcript over.
                    self.finish_setup(&mut step);
                    return Ok(step);
                }
                if matches!(pending, Pending::Models) && !self.listing.is_empty() {
                    // The pages that did arrive are all the listing there
                    // will be.
                    self.models = std::mem::take(&mut self.listing);
                    self.model_events(&mut step);
                }
                if let Pending::QueueDelete(id) = &pending {
                    // Whatever the queue holds now, this entry was not taken
                    // out of it by the request.
                    step.events.push(Event::QueueRemoval(chat::QueueRemoval {
                        id: id.clone(),
                        removed: false,
                    }));
                    self.read_queue(&mut step)?;
                }
                // The actor marks a submitted prompt busy before the server
                // acknowledges it. A rejected start has no turn/completed
                // notification to release that optimistic state. Only repair
                // the lifecycle this request observed: a newer genuine turn
                // must retain its own activity and waiting state.
                if let Pending::Turn { revision, .. } = &pending
                    && *revision == self.lifecycle_revision
                {
                    self.busy = false;
                    self.turn = None;
                    step.events.push(Event::Waiting(false));
                    step.events.push(Event::Status {
                        busy: false,
                        detail: "Turn failed".into(),
                    });
                }
                let fatal = matches!(pending, Pending::Initialize | Pending::Attach);
                let message = format!(
                    "{}: {}",
                    if matches!(pending, Pending::StopChild) {
                        "Codex could not stop the subagent"
                    } else {
                        "Codex RPC error"
                    },
                    error["message"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| "The provider reported an error without a readable message".into())
                );
                step.events
                    .push(if matches!(pending, Pending::History { .. }) {
                        Event::HistoryError(message)
                    } else {
                        Event::Error { message, fatal }
                    });
                if self.execution.as_ref().is_some_and(|execution| {
                    frame["id"].as_str() == Some(execution.request.as_str())
                }) {
                    let execution = self.execution.take().unwrap();
                    execution_end(execution.run, crate::notifications::Kind::Failed, &mut step);
                }
                return Ok(step);
            }
            let result = frame
                .get("result")
                .ok_or("Codex RPC response is missing result/error")?;
            match pending {
                Pending::LoginRecovery {
                    failure,
                    expected,
                    revision,
                } => {
                    if revision != self.lifecycle_revision || self.busy {
                        return Ok(step);
                    }
                    let Ok(identity) = crate::account::codex::parse_account(result) else {
                        step.events.push(Event::LoginVerificationFailed {failure, detail: "Codex sign-in is still unavailable; finish signing in before resuming".into()});
                        return Ok(step);
                    };
                    if identity.email.is_none()
                        || expected
                            .as_ref()
                            .is_some_and(|expected| !expected.same_login(&identity))
                    {
                        step.events.push(Event::LoginVerificationFailed {failure, detail: "Codex signed in to a different or unverifiable account; open Accounts to resolve the login identity".into()});
                        return Ok(step);
                    }
                    step.events.push(Event::LoginRecovered { failure });
                }
                Pending::Initialize => {
                    if let Some(retained) = &self.retained_identity {
                        if result["codexHome"].as_str() != Some(retained.reported_home.as_str()) {
                            return Err("Retained Codex server changed its account identity".into());
                        }
                    } else {
                        crate::agent::observe::socket::verify_account(result, &self.expected_home)?;
                    }
                    let user_agent = result
                        .get("userAgent")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty() && value.len() <= 1024);
                    let identity = user_agent.map(|user_agent| ServerIdentity {
                        user_agent: user_agent.to_owned(),
                        canonical_home: self.expected_home.clone(),
                        reported_home: result["codexHome"]
                            .as_str()
                            .expect("account identity was verified above")
                            .to_owned(),
                    });
                    if self
                        .retained_identity
                        .as_ref()
                        .is_some_and(|retained| Some(retained) != identity.as_ref())
                    {
                        return Err("Retained Codex server identity changed".into());
                    }
                    self.server_identity = identity;
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
                    if self.thread.is_none() {
                        self.cursor = None;
                        self.history_initialized = false;
                    }
                    self.thread = Some(id.into());
                    self.attached = true;
                    self.direct_input = thread["canAcceptDirectInput"].as_bool() == Some(true);
                    self.selected = result["model"].as_str().map(str::to_owned);
                    self.effective_model = self.selected.clone();
                    self.service_tier = result["serviceTier"].as_str().map(str::to_owned);
                    self.service_tier_revision = self.service_tier_revision.saturating_add(1);
                    self.reasoning = result["reasoningEffort"].as_str().map(str::to_owned);
                    self.busy = thread["status"]["type"] == "active";
                    step.events.push(Event::PermissionMode(
                        permission_mode(&result["approvalPolicy"], &result["sandbox"])
                            .map(str::to_owned),
                    ));
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
                    self.call(
                        &mut step,
                        "thread/list",
                        json!({"ancestorThreadId":self.thread,"sourceKinds":["subAgentThreadSpawn"],"limit":32}),
                        Pending::ChildList,
                    )?;
                    if result["initialTurnsPage"].is_object() {
                        self.page(&result["initialTurnsPage"], &mut step, false)?;
                    } else {
                        self.history(&mut step, None)?;
                    }
                    self.finish_setup(&mut step);
                }
                Pending::Models => {
                    for value in array(result, "data")? {
                        let model = Model::from_codex(value).ok_or_else(|| {
                            "Unsupported Codex frame: missing string model".to_owned()
                        })?;
                        let id = model.id.clone();
                        let tiers = value["serviceTiers"].as_array();
                        let fast = if let Some(tiers) = tiers {
                            tiers.iter().any(|tier| tier["id"].as_str() == Some("fast"))
                        } else {
                            value["additionalSpeedTiers"]
                                .as_array()
                                .is_some_and(|tiers| {
                                    tiers.iter().any(|tier| tier.as_str() == Some("fast"))
                                })
                        };
                        if fast {
                            self.fast_models.insert(id.clone());
                        } else {
                            self.fast_models.remove(&id);
                        }
                        if let Some(existing) = self.listing.iter_mut().find(|m| m.id == id) {
                            *existing = model;
                        } else {
                            self.listing.push(model);
                        }
                        if self.listing.len() > MAX_MODELS {
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
                    } else {
                        self.models = std::mem::take(&mut self.listing);
                        self.fast_models
                            .retain(|id| self.models.iter().any(|model| &model.id == id));
                        self.model_events(&mut step);
                    }
                }
                Pending::Skills => {
                    struct ListedSkill<'a> {
                        skill: Skill,
                        path: Option<String>,
                        identity: Option<PathBuf>,
                        scope: &'a str,
                    }
                    // Each skill with the path it is at and the scope that
                    // owns it, which tell apart two skills sharing a name.
                    let mut catalog: Vec<ListedSkill<'_>> = Vec::new();
                    // The cap has to stop the outer walk as well: a catalog
                    // split across many directories would otherwise add one
                    // more skill per directory after the cap was reached.
                    'catalog: for entry in result["data"].as_array().into_iter().flatten() {
                        for value in entry["skills"].as_array().into_iter().flatten() {
                            if catalog.len() >= MAX_SKILLS {
                                break 'catalog;
                            }
                            let Some(name) = value["name"].as_str().filter(|n| !n.is_empty())
                            else {
                                continue;
                            };
                            // A name is a file name in the working tree, so it
                            // is as much provider-controlled text as the
                            // description and is bounded the same way.
                            let name = name.chars().take(MAX_SKILL_NAME).collect::<String>();
                            let path = value["path"].as_str().map(str::to_owned);
                            // Codex can list one skill through both a
                            // symlinked skill directory and its target. Keep
                            // the original path for the typed input, but use
                            // the resolved path to identify the same file.
                            let identity = path.as_ref().map(|path| {
                                std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
                            });
                            // List a physical skill once even if several
                            // searched directories reach it.
                            if value["enabled"].as_bool() == Some(false)
                                || catalog.iter().any(|known| {
                                    known.skill.name == name
                                        && (known.identity.as_ref() == identity.as_ref()
                                            || path.is_none())
                                })
                            {
                                continue;
                            }
                            // The short description is the one written for a
                            // picker; the long one is written for the model
                            // and runs to paragraphs.
                            let description = value["interface"]["shortDescription"]
                                .as_str()
                                .or_else(|| value["shortDescription"].as_str())
                                .or_else(|| value["description"].as_str())
                                .unwrap_or_default();
                            catalog.push(ListedSkill {
                                skill: Skill {
                                    description: description
                                        .lines()
                                        .next()
                                        .unwrap_or_default()
                                        .chars()
                                        .take(160)
                                        .collect(),
                                    // Codex acts on a skill the message names
                                    // with a leading dollar sign. It has no
                                    // command for one, so there is nothing to
                                    // dispatch.
                                    invocation: format!("${name}"),
                                    name,
                                },
                                path,
                                identity,
                                scope: value["scope"].as_str().unwrap_or_default(),
                            });
                        }
                    }
                    // A name two skills share loads neither, so each is named
                    // by a link to its own file instead, which Codex reads
                    // too, and its scope says which one it is.
                    let shared = |name: &str| {
                        catalog
                            .iter()
                            .filter(|listed| listed.skill.name == name)
                            .nth(1)
                            .is_some()
                    };
                    let skills = catalog
                        .iter()
                        .map(|listed| {
                            let skill = &listed.skill;
                            let scope = listed.scope;
                            match &listed.path {
                                Some(path) if shared(&skill.name) && !path.contains(')') => Skill {
                                    invocation: format!("[${}]({path})", skill.name),
                                    description: {
                                        let same_scope = catalog
                                            .iter()
                                            .filter(|other| {
                                                other.skill.name == skill.name
                                                    && other.scope == scope
                                            })
                                            .nth(1)
                                            .is_some();
                                        let source = (scope.is_empty() || same_scope).then(|| {
                                            skill_source(
                                                path,
                                                catalog
                                                    .iter()
                                                    .filter(|other| other.skill.name == skill.name)
                                                    .filter_map(|other| other.path.as_deref())
                                                    .filter(|other_path| {
                                                        *other_path != path.as_str()
                                                    }),
                                            )
                                        });
                                        [
                                            (!scope.is_empty()).then_some(scope),
                                            source.as_deref(),
                                            Some(skill.description.as_str()),
                                        ]
                                        .into_iter()
                                        .flatten()
                                        .filter(|part| !part.is_empty())
                                        .collect::<Vec<_>>()
                                        .join(" · ")
                                    },
                                    name: skill.name.clone(),
                                },
                                _ => skill.clone(),
                            }
                        })
                        .collect();
                    self.skill_paths = catalog
                        .into_iter()
                        .filter_map(|listed| Some((listed.skill.name, listed.path?)))
                        .collect();
                    step.events.push(Event::Skills(skills));
                    self.finish_setup(&mut step);
                    if std::mem::take(&mut self.skills_stale) {
                        self.skills(&mut step)?;
                    }
                }
                Pending::History { earlier } => {
                    self.page(result, &mut step, earlier)?;
                    self.finish_history(&mut step)?;
                }
                Pending::FastMode {
                    enabled,
                    revision,
                    model,
                } => {
                    // A settings notification can confirm or supersede this
                    // request before its acknowledgement arrives.
                    if revision == self.service_tier_revision {
                        self.service_tier = enabled.then(|| "fast".into());
                        if model.is_some() {
                            self.effective_model = model;
                        }
                        self.service_tier_revision = self.service_tier_revision.saturating_add(1);
                        self.fast_mode_event(&mut step);
                        step.events.push(Event::Capabilities(self.capabilities()));
                    }
                }
                Pending::Turn {
                    revision,
                    model,
                    tier,
                } => {
                    self.materialized = true;
                    let id = string(&result["turn"], "id")?;
                    self.remember_turn_pricing(id, model, tier);
                    // Notifications can finish this turn or start another before
                    // its RPC reply. An acknowledgement only fills missing state.
                    if revision == self.lifecycle_revision {
                        self.turn = Some(id.into());
                        self.busy = true;
                        self.subagent_status = Some(ToolStatus::Running);
                        step.events.push(Event::Waiting(false));
                        step.events.push(Event::Status {
                            busy: true,
                            detail: "Codex turn started".into(),
                        });
                    }
                    if let Some(execution) = self.execution.as_mut()
                        && frame["id"].as_str() == Some(execution.request.as_str())
                    {
                        let turn = string(&result["turn"], "id")?.to_owned();
                        execution.turn = Some(turn.clone());
                        let completed = execution.early.remove(&turn);
                        execution.early.clear();
                        execution.early_bytes = 0;
                        if let Some(completed) = completed {
                            self.finish_execution(&completed, &mut step)?;
                            if !self.busy || self.turn.as_deref() == Some(turn.as_str()) {
                                self.busy = false;
                                self.turn = None;
                                step.events.push(Event::Status {
                                    busy: false,
                                    detail: turn_detail(&completed["status"]),
                                });
                            }
                        }
                    }
                }
                // The one visible effect of compaction is the context figure,
                // and no notification announces that it has moved, so the
                // acknowledgement re-reads the thread the way a refresh does.
                // Without this the reader pressed a control and the gauge it
                // exists to move kept its old reading until some later frame
                // happened to carry a new one.
                Pending::Compact => self.reread(&mut step)?,
                Pending::QueueAdd => self.read_queue(&mut step)?,
                Pending::QueueDelete(id) => {
                    step.events.push(Event::QueueRemoval(chat::QueueRemoval {
                        id,
                        removed: true,
                    }));
                    self.read_queue(&mut step)?;
                }
                Pending::Fork => {
                    let id = string(&result["thread"], "id")?;
                    if uuid::Uuid::parse_str(id).is_err() || self.thread.as_deref() == Some(id) {
                        return Err("Codex did not create a distinct conversation".into());
                    }
                    self.call(
                        &mut step,
                        "thread/unsubscribe",
                        json!({"threadId":id}),
                        Pending::Other,
                    )?;
                    step.events.push(Event::Forked(id.into()));
                }
                Pending::ChildList => {
                    let mut remaining = result["data"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .take(32)
                        .collect::<Vec<_>>();
                    while !remaining.is_empty() {
                        let before = remaining.len();
                        let mut deferred = Vec::new();
                        for thread in remaining {
                            // Older servers can ignore an unknown list filter.
                            // Require explicit lineage before subscribing or routing
                            // approvals from an account-wide result.
                            let Some(parent) = thread["parentThreadId"].as_str() else {
                                continue;
                            };
                            if self.thread.as_deref() != Some(parent)
                                && !self.children.contains_key(parent)
                            {
                                deferred.push(thread);
                                continue;
                            }
                            if let Some(id) = thread["id"].as_str() {
                                self.discover_child(id, thread, &mut step)?;
                            }
                        }
                        if deferred.len() == before {
                            break;
                        }
                        remaining = deferred;
                    }
                }
                Pending::ChildResume(id) => {
                    // Overflow abandons only hydration, retaining live content.
                    if !self.child_buffer.contains_key(&id) {
                        return Ok(step);
                    }
                    if result["thread"]["id"].as_str() != Some(id.as_str()) {
                        return Err("Codex returned a different child thread".into());
                    }
                    if let Some(child) = self.children.get_mut(&id) {
                        let mut content = Step::default();
                        child.selected = result["model"]
                            .as_str()
                            .filter(|model| !model.is_empty())
                            .map(str::to_owned)
                            .or_else(|| child.selected.clone());
                        child.thread_state(&result["thread"], &mut content)?;
                        child.ready = true;
                        child.materialized = true;
                        if result["initialTurnsPage"].is_object() {
                            child.page(&result["initialTurnsPage"], &mut content, false)?;
                        } else {
                            for turn in result["thread"]["turns"].as_array().into_iter().flatten() {
                                for value in turn["items"].as_array().into_iter().flatten() {
                                    for item in parse_item(value, true, string(turn, "id")?)? {
                                        content.events.push(Event::Item(item));
                                    }
                                }
                            }
                            child.child_history_outcome(
                                result["thread"]["turns"]
                                    .as_array()
                                    .and_then(|turns| turns.last()),
                                &mut content,
                            );
                            if result["thread"]["turns"]
                                .as_array()
                                .is_none_or(Vec::is_empty)
                            {
                                child.history(&mut content, None)?;
                            }
                        }
                        self.child_step(&id, content, &mut step)?;
                    }
                    let buffered = if self
                        .children
                        .get(&id)
                        .is_some_and(|child| child.history_loading)
                    {
                        Vec::new()
                    } else {
                        self.child_buffer.remove(&id).unwrap_or_default()
                    };
                    for frame in buffered {
                        if let Some(child) = self.children.get_mut(&id) {
                            let content = child.receive(frame)?;
                            self.child_step(&id, content, &mut step)?;
                        }
                    }
                }
                Pending::QueueList => {
                    if let Err(message) = self.queue_page(result, &mut step) {
                        self.queue_listing.clear();
                        step.events.push(Event::Error {
                            message,
                            fatal: false,
                        });
                    }
                }
                Pending::ChildRpc { .. } => unreachable!("child responses routed above"),
                // The child's own turn completion reports how it ended.
                Pending::StopChild | Pending::Prediction => {}
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
        if method == "thread/started" {
            let metadata = &params["thread"];
            let parent = metadata["parentThreadId"].as_str().or_else(|| {
                metadata["source"]["subAgent"]["thread_spawn"]["parent_thread_id"].as_str()
            });
            if parent.is_some_and(|id| {
                self.thread.as_deref() == Some(id) || self.children.contains_key(id)
            }) && let Some(id) = thread
            {
                self.discover_child(id, metadata, &mut step)?;
                // Discovery already consumed this metadata. Replaying its
                // initial active state after hydration could revive a child
                // that finished while the read was pending.
                return Ok(step);
            }
        }
        if thread.is_some() && thread != self.thread.as_deref() {
            if let Some(id) = thread
                && self.children.contains_key(id)
            {
                if method == "serverRequest/resolved" {
                    let request_id = rpc_id(&params["requestId"])?;
                    if self.requests.get(&request_id).is_some_and(|request| {
                        request.frame["params"]["threadId"].as_str() == Some(id)
                    }) {
                        self.requests.remove(&request_id);
                        step.events.push(Event::RequestResolved { id: request_id });
                    }
                    return Ok(step);
                }
                if method == "thread/closed" {
                    let requests = self
                        .requests
                        .iter()
                        .filter(|(_, request)| {
                            request.frame["params"]["threadId"].as_str() == Some(id)
                        })
                        .map(|(id, _)| id.clone())
                        .collect::<Vec<_>>();
                    for request in requests {
                        self.requests.remove(&request);
                        step.events.push(Event::RequestResolved { id: request });
                    }
                }
                if params["item"]["type"] == "collabAgentToolCall" {
                    for receiver in params["item"]["receiverThreadIds"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .take(32)
                    {
                        self.discover_child(
                            receiver,
                            &params["item"]["agentsStates"][receiver],
                            &mut step,
                        )?;
                    }
                }
                if let Some(buffer) = self.child_buffer.get_mut(id) {
                    if buffer.len() < 128
                        && buffer
                            .iter()
                            .map(|frame| frame.to_string().len())
                            .sum::<usize>()
                            + frame.to_string().len()
                            <= 512 * 1024
                    {
                        buffer.push(frame.clone());
                        return Ok(step);
                    }
                    for buffered in self.child_buffer.remove(id).unwrap_or_default() {
                        let content = self.children.get_mut(id).unwrap().receive(buffered)?;
                        self.child_step(id, content, &mut step)?;
                    }
                }
                let content = self.children.get_mut(id).unwrap().receive(frame.clone())?;
                self.child_step(id, content, &mut step)?;
            }
            return Ok(step);
        }
        match method {
            "thread/started" => {
                if self.ready {
                    self.thread_state(&params["thread"], &mut step)?;
                }
            }
            "thread/status/changed" => {
                self.observe_thread_status(&params["status"]);
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
            "thread/prediction/updated" => {
                if !self.busy
                    && params["sourceTurnId"].as_str() == self.suggestion_turn.as_deref()
                    && self.suggestion_turn.is_some()
                {
                    self.suggestion_turn = None;
                    if params["result"]["type"] == "completed"
                        && let Some(text) = params["result"]["text"].as_str()
                        && chat::PromptSuggestion::valid_text(text)
                    {
                        step.events.push(Event::PromptSuggestion {
                            session_id: self.thread.clone().unwrap(),
                            source: string(params, "sourceTurnId")?.into(),
                            text: text.into(),
                        });
                    }
                }
            }
            "thread/name/updated" => step.events.push(Event::Session {
                id: self.thread.clone().ok_or("Missing Codex thread")?,
                title: params["threadName"].as_str().map(str::to_owned),
            }),
            "thread/settings/updated" => {
                let settings = &params["threadSettings"];
                if settings.get("model").is_some() {
                    self.effective_model = settings["model"].as_str().map(str::to_owned);
                }
                if settings.get("serviceTier").is_some() {
                    self.service_tier = settings["serviceTier"].as_str().map(str::to_owned);
                    self.service_tier_revision = self.service_tier_revision.saturating_add(1);
                    self.fast_mode_event(&mut step);
                    step.events.push(Event::Capabilities(self.capabilities()));
                }
                let mode = permission_mode(
                    &params["threadSettings"]["approvalPolicy"],
                    &params["threadSettings"]["sandboxPolicy"],
                );
                step.events
                    .push(Event::PermissionMode(mode.map(str::to_owned)));
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
                if method == "turn/started" {
                    self.suggestion_turn = None;
                    step.events.push(Event::TurnStarted);
                }
                self.materialized = true;
                let turn = &params["turn"];
                if method == "turn/completed" {
                    use crate::notifications::Kind;
                    let outcome = match turn["status"].as_str() {
                        Some("completed") => Some(Kind::Succeeded),
                        Some("failed") => Some(Kind::Failed),
                        Some("interrupted") => Some(Kind::Cancelled),
                        _ => None,
                    };
                    if let Some(outcome) = outcome {
                        step.events.push(Event::TurnFinished {
                            key: Some(format!(
                                "codex:{}:{}",
                                string(params, "threadId")?,
                                string(turn, "id")?
                            )),
                            outcome,
                        });
                    }
                }
                let turn_id = string(turn, "id")?;
                if method == "turn/started" {
                    let (model, tier) = self
                        .pending
                        .values()
                        .find_map(|pending| match pending {
                            Pending::Turn { model, tier, .. } => {
                                Some((model.clone(), tier.clone()))
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| {
                            (self.effective_model.clone(), self.service_tier.clone())
                        });
                    self.remember_turn_pricing(turn_id, model, tier);
                    self.prediction_turn = Some(turn_id.into());
                }
                let prediction_turn =
                    method == "turn/completed" && self.prediction_turn.as_deref() == Some(turn_id);
                if prediction_turn {
                    self.prediction_turn = None;
                }
                let current =
                    method == "turn/started" || self.turn.as_deref().is_none_or(|id| id == turn_id);
                if current {
                    if method == "turn/completed" {
                        step.events.push(Event::TurnSettled);
                    }
                    self.lifecycle_revision = self.lifecycle_revision.saturating_add(1);
                    self.busy = method == "turn/started";
                    self.subagent_waiting = false;
                    self.subagent_detail = turn_detail(&turn["status"]);
                    self.subagent_status = if self.busy {
                        Some(ToolStatus::Running)
                    } else {
                        agent_status(&turn["status"])
                    };
                    step.events.push(Event::Waiting(false));
                    self.turn = self.busy.then(|| turn_id.to_owned());
                }
                for value in array(turn, "items")? {
                    for item in parse_item(value, method == "turn/completed", string(turn, "id")?)?
                    {
                        if method == "turn/started" && self.tracks(value, &item) {
                            self.active_items.insert(item.id.clone(), item.clone());
                        }
                        step.events.push(Event::Item(item));
                    }
                }
                // Non-blocking questions can outlive their originating turn.
                // Only serverRequest/resolved or thread closure invalidates them.
                if current {
                    if !self.busy {
                        self.active_items.clear();
                        self.output_streams.clear();
                        self.output_windows.clear();
                    }
                    step.events.push(Event::Status {
                        busy: self.busy,
                        detail: turn_detail(&turn["status"]),
                    });
                }
                if turn["error"].is_object() {
                    step.events.push(Event::Error {
                        message: turn["error"]["message"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| turn["error"].to_string()),
                        fatal: false,
                    });
                }
                if method == "turn/completed" {
                    self.finish_execution(turn, &mut step)?;
                    if current
                        && prediction_turn
                        && turn["status"] == "completed"
                        && !self.is_child
                        && !self.predictions_unsupported
                        && self.pending.len() < MAX_PENDING
                        && !self
                            .pending
                            .values()
                            .any(|pending| matches!(pending, Pending::Prediction))
                    {
                        self.suggestion_turn = Some(turn_id.into());
                        self.call(
                            &mut step,
                            "thread/prediction/request",
                            json!({"threadId":self.thread,"sourceTurnId":turn_id}),
                            Pending::Prediction,
                        )?;
                    }
                }
            }
            "item/started" | "item/completed" => {
                let done = method == "item/completed";
                if params["item"]["type"] == "collabAgentToolCall" {
                    let value = &params["item"];
                    for id in value["receiverThreadIds"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .take(32)
                    {
                        self.discover_child(id, &value["agentsStates"][id], &mut step)?;
                    }
                }
                for item in parse_item(&params["item"], done, string(params, "turnId")?)? {
                    if done {
                        self.active_items.remove(&item.id);
                        self.output_streams.remove(&item.id);
                        self.output_windows.remove(&item.id);
                    } else if self.tracks(&params["item"], &item) {
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
                            params["message"].as_str().unwrap_or_default().into()
                        } else if method.starts_with("item/commandExecution") {
                            // A command writes for a terminal, and a delta
                            // may end anywhere in what it wrote.
                            std::borrow::Cow::Owned(
                                self.output_streams
                                    .entry(id.clone())
                                    .or_default()
                                    .push(string(params, "delta")?),
                            )
                        } else {
                            std::borrow::Cow::Borrowed(string(params, "delta")?)
                        };
                        let delta = delta.as_ref();
                        let limit =
                            MAX_ITEM_BYTES.saturating_sub(bytes.saturating_sub(output.len()));
                        let offset = output.len();
                        let window = self.output_windows.entry(id.clone()).or_default();
                        let preserves_prefix =
                            window.omitted == 0 && offset.saturating_add(delta.len()) <= limit;
                        if append_display_output(output, delta, limit, window) {
                            if preserves_prefix {
                                step.events.push(Event::OutputDelta {
                                    id,
                                    offset,
                                    delta: output[offset..].to_owned(),
                                });
                            } else {
                                step.events.push(Event::Item(item.clone()));
                            }
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
                let tokens = |value: &Value| chat::pricing::Tokens {
                    input: value["inputTokens"].as_u64().unwrap_or(0),
                    cached_input: value["cachedInputTokens"].as_u64().unwrap_or(0),
                    cache_write_input: value["cacheWriteInputTokens"].as_u64().unwrap_or(0),
                    output: value["outputTokens"].as_u64().unwrap_or(0),
                };
                let observed = self
                    .turn_pricing
                    .iter()
                    .rev()
                    .find(|(turn, _, _)| Some(turn.as_str()) == params["turnId"].as_str());
                let cost_estimate = self.pricing.update(
                    observed.and_then(|(_, model, _)| model.as_deref()),
                    observed.and_then(|(_, _, tier)| tier.as_deref()),
                    tokens(&usage["total"]),
                    tokens(&usage["last"]),
                );
                step.events.push(Event::Usage(Usage {
                    cost_estimate,
                    input_tokens: usage["total"]["inputTokens"].as_u64().unwrap_or(0),
                    output_tokens: usage["total"]["outputTokens"].as_u64().unwrap_or(0),
                    context_tokens: usage["last"]["totalTokens"].as_u64().unwrap_or(0),
                    context_limit: usage["modelContextWindow"].as_u64().unwrap_or(0),
                    cost_microusd: None,
                }));
            }
            "thread/closed" | "thread/deleted" => {
                self.ready = false;
                self.attached = false;
                self.subagent_waiting = false;
                self.subagent_detail = "Agent disconnected".into();
                step.events.push(Event::Ready(false));
                self.busy = false;
                self.subagent_status = Some(
                    self.subagent_status
                        .filter(|status| *status != ToolStatus::Running)
                        .unwrap_or(ToolStatus::Stopped),
                );
                self.lifecycle_revision = self.lifecycle_revision.saturating_add(1);
                self.invalidate(&mut step);
                step.events.push(Event::Error {
                    message: format!("Codex {method}; reconnect explicitly"),
                    fatal: true,
                });
            }
            "error" => {
                let message = params["error"]["message"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "Codex reported an error without a readable message".into());
                // A retry describes work still in progress. Putting it in
                // Snapshot.error leaves a recovered turn marked as failed.
                if params["willRetry"] == true {
                    step.events.push(session_notice(message));
                } else {
                    step.events.push(Event::Error {
                        message,
                        fatal: false,
                    });
                }
            }
            "warning" | "configWarning" | "guardianWarning" | "deprecationNotice" => {
                let summary = params["message"]
                    .as_str()
                    .or_else(|| params["summary"].as_str())
                    .filter(|text| !text.trim().is_empty());
                let mut message = summary
                    .map(str::to_owned)
                    .unwrap_or_else(|| "Codex reported a configuration warning; check the provider log for details".into());
                if summary.is_some() {
                    for key in ["details", "path"] {
                        if let Some(text) = params[key].as_str().filter(|text| !text.is_empty()) {
                            message.push_str("\n\n");
                            message.push_str(text);
                        }
                    }
                }
                step.events.push(session_notice(message));
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
                started_at: None,
                at: None,
            })),
            "turn/plan/updated" => step.events.push(Event::Plan {
                steps: plan_steps(&params["plan"]),
            }),
            "thread/queue/changed" => self.read_queue(&mut step)?,
            // The catalog is a cache the server watches for itself, so a
            // change notification is an invalidation and nothing more.
            "skills/changed" => self.skills(&mut step)?,
            // The account this server runs on, which the surface keeps apart
            // from the transcript. A rate-limit update is sparse and is
            // forwarded as what it names, for the surface to merge.
            "account/rateLimits/updated" => {
                if let Some(update) = account::codex::parse_update(params) {
                    step.events
                        .push(Event::AccountUsage(LiveReport::CodexLimits(update)));
                }
            }
            "account/updated" => step.events.push(Event::AccountUsage(LiveReport::CodexLogin(
                account::codex::parse_login_update(params),
            ))),
            "item/reasoning/summaryPartAdded" | "thread/compacted" => {}
            // Auxiliary app-server notifications are not transcript data.
            // Unknown item types are shown as tool disclosures and unknown
            // requests are visibly rejected above.
            _ => {}
        }
        Ok(step)
    }
}

/// Advisory provider messages belong to the transcript, at their arrival
/// position. Each occurrence has its own identity so later work and retries
/// cannot move an old notice to the end or overwrite a previous one.
fn session_notice(text: String) -> Event {
    Event::Item(Item {
        id: format!("codex-notice:{}", uuid::Uuid::new_v4()),
        role: Role::System,
        content: ItemContent::Text {
            text,
            thinking: false,
            phase: Phase::Commentary,
        },
        started_at: None,
        at: None,
    })
}
impl Codex {
    fn observe_thread_status(&mut self, status: &Value) {
        self.lifecycle_revision = self.lifecycle_revision.saturating_add(1);
        self.busy = status["type"] == "active";
        self.subagent_waiting = waiting_on_user(status);
        self.subagent_detail = status_detail(status);
        match status["type"].as_str() {
            Some("active") => self.subagent_status = Some(ToolStatus::Running),
            Some("systemError") => self.subagent_status = Some(ToolStatus::Failed),
            Some("idle")
                if self
                    .subagent_status
                    .is_none_or(|value| value == ToolStatus::Running) =>
            {
                self.subagent_status = Some(ToolStatus::Succeeded);
            }
            _ => {}
        }
    }
    fn thread_state(&mut self, thread: &Value, step: &mut Step) -> Result<(), String> {
        let id = string(thread, "id")?;
        if self.thread.as_deref() != Some(id) {
            return Err("Codex metadata belongs to a different thread".into());
        }
        self.direct_input = thread["canAcceptDirectInput"].as_bool() == Some(true);
        if !self.selection_dirty
            && let Some(model) = thread["model"].as_str().filter(|model| !model.is_empty())
        {
            self.selected = Some(model.to_owned());
        }
        self.observe_thread_status(&thread["status"]);
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
fn agent_status(status: &Value) -> Option<ToolStatus> {
    match status.as_str().or_else(|| status["type"].as_str()) {
        Some("completed" | "idle" | "shutdown") => Some(ToolStatus::Succeeded),
        Some("errored" | "failed" | "notFound" | "systemError") => Some(ToolStatus::Failed),
        Some("interrupted") => Some(ToolStatus::Stopped),
        Some("running" | "pendingInit" | "active" | "inProgress") => Some(ToolStatus::Running),
        _ => None,
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
/// Running items the app-server can still write into. A live turn's history
/// also contains settled tools; retaining those exhausts the concurrency
/// bound when a long conversation is resumed or refreshed.
///
/// Only the item types Codex has a delta notification for qualify. The rest
/// are never written into, and the ones that carry no status of their own,
/// `sleep` and `subAgentActivity` among them, read as running for as long as
/// their turn does, so a turn that orchestrates agents holds hundreds.
fn streams_updates(value: &Value, content: &ItemContent) -> bool {
    matches!(
        value["type"].as_str(),
        Some("commandExecution" | "fileChange" | "mcpToolCall")
    ) && content.status() == Some(ToolStatus::Running)
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
/// view cannot already read off the command itself. The label is what the
/// action was applied to; the view supplies the verb, from the action.
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
                    path.map(str::to_owned).unwrap_or_default(),
                );
            }
            Some("search") => {
                return (
                    CommandAction::Search,
                    match (action["query"].as_str(), path) {
                        (Some(query), Some(path)) => format!("{query} in {path}"),
                        (Some(query), None) => query.to_owned(),
                        (None, Some(path)) => path.to_owned(),
                        (None, None) => String::new(),
                    },
                );
            }
            Some("listFiles") => {
                return (
                    CommandAction::List,
                    path.map(str::to_owned).unwrap_or_default(),
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
        started_at: None,
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
            started_at: None,
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
            started_at: None,
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
                        started_at: None,
                        at: None,
                    }),
                    // A skill or a mention part names what the text beside it
                    // already names, so it adds nothing to show.
                    "skill" | "mention" => {}
                    // An audio part has no rendering of its own, and neither
                    // has a part type this build cannot name,
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
                    // The whole output, so a line the command redrew while
                    // it ran is kept only as it was last drawn.
                    output: super::terminal_output::display(
                        value["aggregatedOutput"].as_str().unwrap_or("").into(),
                    ),
                },
                started_at: None,
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
            started_at: None,
            at: None,
        }],
        "mcpToolCall" => images::tool_result(
            id,
            status,
            format!("{} / {}", string(value, "server")?, string(value, "tool")?),
            value["arguments"].to_string(),
            if !value["error"].is_null() {
                &value["error"]
            } else if !value["result"].is_null() {
                &value["result"]
            } else {
                &Value::Null
            },
        ),
        "dynamicToolCall" => images::tool_result(
            id,
            status,
            string(value, "tool")?.into(),
            value["arguments"].to_string(),
            &value["contentItems"],
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
            started_at: None,
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
            started_at: None,
            at: None,
        }],
        "imageGeneration" => {
            let path = value["savedPath"]
                .as_str()
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    value["result"]
                        .as_str()
                        .filter(|data| !data.is_empty())
                        .map(|data| {
                            if data.starts_with("data:") {
                                data.to_owned()
                            } else {
                                format!("data:image/png;base64,{data}")
                            }
                        })
                });
            match path {
                Some(path)
                    if status == ToolStatus::Succeeded
                        && path.len() <= chat::MAX_INLINE_IMAGE_URL_BYTES =>
                {
                    vec![Item {
                        id: id.into(),
                        role: Role::Assistant,
                        started_at: None,
                        at: None,
                        content: ItemContent::Image {
                            path,
                            description: "Generated image".into(),
                        },
                    }]
                }
                _ => tool(
                    "Image generation".into(),
                    value["revisedPrompt"].as_str().unwrap_or("").into(),
                    "No displayable image was returned.".into(),
                ),
            }
        }
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
        let limit = if matches!(&item.content, ItemContent::Image { path, .. } if path.starts_with("data:image/"))
        {
            chat::MAX_INLINE_IMAGE_URL_BYTES + 1024
        } else {
            MAX_ITEM_BYTES
        };
        if bytes > MAX_ITEM_BYTES {
            if let ItemContent::Tool { output, .. } | ItemContent::Command { output, .. } =
                &mut item.content
            {
                let limit = MAX_ITEM_BYTES.saturating_sub(bytes.saturating_sub(output.len()));
                // Completed items replace streamed items, so apply the same bound.
                let full = std::mem::take(output);
                append_display_output(output, &full, limit, &mut OutputWindow::default());
            }
        }
        if item.content.byte_len() > limit {
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

/// A skill a message names, as Codex finds it in the text.
#[derive(Debug, PartialEq, Eq)]
enum Mention<'a> {
    /// `[$name](path)`, naming the skill at `path`.
    Linked(&'a str),
    /// `$name`.
    Named(&'a str),
}

/// The skills `text` names, in the order it names them, read as Codex's own
/// `extract_tool_mentions` reads them: anywhere in the text, a name being
/// the characters Codex allows in one and never a common environment
/// variable.
fn skill_mentions(text: &str) -> Vec<Mention<'_>> {
    fn name_end(text: &str, start: usize) -> Option<usize> {
        let length = text[start..]
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b':'))
            .count();
        (length > 0).then_some(start + length)
    }
    fn environment(name: &str) -> bool {
        [
            "PATH",
            "HOME",
            "USER",
            "SHELL",
            "PWD",
            "TMPDIR",
            "TEMP",
            "TMP",
            "LANG",
            "TERM",
            "XDG_CONFIG_HOME",
        ]
        .iter()
        .any(|variable| variable.eq_ignore_ascii_case(name))
    }
    let bytes = text.as_bytes();
    let mut mentions = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'['
            && bytes.get(index + 1) == Some(&b'$')
            && let Some(end) = name_end(text, index + 2)
            && bytes.get(end) == Some(&b']')
        {
            let open = end
                + 1
                + text[end + 1..]
                    .bytes()
                    .take_while(u8::is_ascii_whitespace)
                    .count();
            // A link without a path is read as the bare name inside it.
            if bytes.get(open) == Some(&b'(')
                && let Some(close) = text[open..].find(')')
                && let path = text[open + 1..open + close].trim()
                && !path.is_empty()
            {
                if !environment(&text[index + 2..end]) {
                    mentions.push(Mention::Linked(path));
                }
                index = open + close + 1;
                continue;
            }
        }
        if bytes[index] == b'$'
            && let Some(end) = name_end(text, index + 1)
        {
            let name = &text[index + 1..end];
            if !environment(name) {
                mentions.push(Mention::Named(name));
            }
            index = end;
            continue;
        }
        index += 1;
    }
    mentions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_output_preserves_first_last_lines_and_exact_omission_bytes() {
        let source = (1..=200000)
            .map(|line| format!("{line}:🦀\n"))
            .collect::<String>();
        let mut output = String::new();
        let mut window = OutputWindow::default();
        let mut start = 0;
        while start < source.len() {
            let end = source.floor_char_boundary((start + 1024).min(source.len()));
            append_display_output(
                &mut output,
                &source[start..end],
                MAX_ITEM_BYTES,
                &mut window,
            );
            assert!(output.len() <= MAX_ITEM_BYTES);
            start = end;
        }
        assert!(output.starts_with("1:🦀\n"));
        assert!(output.ends_with("200000:🦀\n"));
        let marker = format!("\n[... {} bytes omitted ...]\n", window.omitted);
        assert!(output.contains(&marker));
        assert_eq!(source.len(), output.len() - marker.len() + window.omitted);
    }

    #[test]
    fn absent_or_failed_predictions_and_observed_child_turns_never_produce_prompts() {
        for result in [
            json!({"type":"failed"}),
            json!({"type":"completed","text":null}),
            json!({"type":"completed","text":""}),
            json!({"type":"completed","text":"x".repeat(chat::PromptSuggestion::MAX_BYTES + 1)}),
        ] {
            let mut current = adapter();
            current.receive(json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"turn","status":"inProgress","items":[]}}})).unwrap();
            current.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"turn","status":"completed","items":[]}}})).unwrap();
            let absent = current.receive(json!({"method":"thread/prediction/updated","params":{"threadId":"thread-exact","sourceTurnId":"turn","result":result}})).unwrap();
            assert!(absent.events.is_empty());
            assert!(absent.writes.is_empty());
            assert!(current.ready);
        }
        let mut child = adapter();
        child.is_child = true;
        child.receive(json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"turn","status":"inProgress","items":[]}}})).unwrap();
        let finished = child.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"turn","status":"completed","items":[]}}})).unwrap();
        assert!(
            !finished
                .writes
                .iter()
                .any(|frame| frame["method"] == "thread/prediction/request")
        );
    }

    #[test]
    fn next_prompt_prediction_is_correlated_and_legacy_errors_are_quiet() {
        let mut current = adapter();
        let completion = |id: &str| json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":id,"status":"completed","items":[]}}});
        let start = |id: &str| json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":id,"status":"inProgress","items":[]}}});
        current.receive(start("turn-one")).unwrap();
        let finished = current.receive(completion("turn-one")).unwrap();
        let request = finished
            .writes
            .iter()
            .find(|frame| frame["method"] == "thread/prediction/request")
            .unwrap();
        assert_eq!(request["params"]["sourceTurnId"], "turn-one");
        let prediction = |thread: &str, turn: &str| json!({"method":"thread/prediction/updated","params":{"threadId":thread,"sourceTurnId":turn,"result":{"type":"completed","text":"Check the resulting diff"}}});
        assert!(
            current
                .receive(prediction("another-thread", "turn-one"))
                .unwrap()
                .events
                .is_empty()
        );
        assert!(
            current
                .receive(prediction("thread-exact", "old-turn"))
                .unwrap()
                .events
                .is_empty()
        );
        assert!(
            matches!(current.receive(prediction("thread-exact", "turn-one")).unwrap().events.as_slice(), [Event::PromptSuggestion { text, .. }] if text == "Check the resulting diff")
        );
        current
            .receive(json!({"id":request["id"],"result":{}}))
            .unwrap();
        assert!(
            !current
                .receive(completion("old-turn"))
                .unwrap()
                .writes
                .iter()
                .any(|frame| frame["method"] == "thread/prediction/request")
        );
        current.receive(start("turn-two")).unwrap();
        current.receive(completion("turn-two")).unwrap();
        current
            .command(Command::Prompt {
                text: "My next message".into(),
                attachments: vec![],
                mode: PromptMode::Send,
            })
            .unwrap();
        assert!(
            current
                .receive(prediction("thread-exact", "turn-two"))
                .unwrap()
                .events
                .is_empty()
        );

        let mut legacy = adapter();
        legacy.receive(start("turn-one")).unwrap();
        let finished = legacy.receive(completion("turn-one")).unwrap();
        let request = finished
            .writes
            .iter()
            .find(|frame| frame["method"] == "thread/prediction/request")
            .unwrap();
        let error = legacy
            .receive(
                json!({"id":request["id"],"error":{"code":-32601,"message":"Method not found"}}),
            )
            .unwrap();
        assert!(error.events.is_empty());
        assert!(error.writes.is_empty());
        assert!(
            !legacy
                .receive(completion("turn-two"))
                .unwrap()
                .writes
                .iter()
                .any(|frame| frame["method"] == "thread/prediction/request")
        );
    }

    #[test]
    fn rejected_turn_start_releases_optimistic_activity_for_sign_in_recovery() {
        let mut adapter = adapter();
        let submitted = adapter
            .command(Command::Prompt {
                text: "first attempt".into(),
                attachments: vec![],
                mode: PromptMode::Send,
            })
            .unwrap();
        let request = submitted
            .writes
            .iter()
            .find(|frame| frame["method"] == "turn/start")
            .unwrap();
        let rejected = adapter.receive(json!({"id":request["id"],"error":{"code":-32000,"message":"Your access token could not be refreshed. Please sign in again."}})).unwrap();
        assert!(
            rejected
                .events
                .iter()
                .any(|event| matches!(event, Event::Waiting(false)))
        );
        assert!(rejected.events.iter().any(|event| matches!(event, Event::Status { busy: false, detail } if detail == "Turn failed")));
        assert!(rejected.events.iter().any(|event| matches!(event, Event::Error { message, fatal: false } if chat::requires_sign_in(crate::agent::Provider::Codex, message))));
        assert!(!adapter.busy);
        assert!(adapter.turn.is_none());
        assert!(
            adapter
                .command(Command::Prompt {
                    text: "explicit new attempt".into(),
                    attachments: vec![],
                    mode: PromptMode::Send,
                })
                .is_ok()
        );
    }

    #[test]
    fn rejected_turn_start_does_not_settle_a_newer_provider_turn() {
        for completed in [false, true] {
            let mut adapter = adapter();
            let submitted = adapter
                .command(Command::Prompt {
                    text: "old attempt".into(),
                    attachments: vec![],
                    mode: PromptMode::Send,
                })
                .unwrap();
            let request = submitted
                .writes
                .iter()
                .find(|frame| frame["method"] == "turn/start")
                .unwrap();
            adapter.receive(json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"newer-turn","status":"inProgress","items":[]}}})).unwrap();
            if completed {
                adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"newer-turn","status":"completed","items":[]}}})).unwrap();
            }
            let revision = adapter.lifecycle_revision;
            let rejected = adapter.receive(json!({"id":request["id"],"error":{"code":-32000,"message":"Old request failed"}})).unwrap();
            assert!(!rejected.events.iter().any(|event| matches!(
                event,
                Event::Status { .. }
                    | Event::Waiting(_)
                    | Event::TurnSettled
                    | Event::TurnFinished { .. }
            )));
            assert_eq!(adapter.lifecycle_revision, revision);
            assert_eq!(adapter.busy, !completed);
            assert_eq!(
                adapter.turn.as_deref(),
                (!completed).then_some("newer-turn")
            );
        }
    }

    #[test]
    fn attention_requires_explicit_settlement_of_the_bound_thread() {
        use crate::notifications::Kind;
        for (status, expected) in [
            ("completed", Some(Kind::Succeeded)),
            ("failed", Some(Kind::Failed)),
            ("interrupted", Some(Kind::Cancelled)),
            ("future", None),
        ] {
            let mut adapter = adapter();
            let other = adapter.receive(json!({"method":"turn/completed","params":{"threadId":"other","turn":{"id":"turn","status":status,"items":[]}}})).unwrap();
            assert!(
                !other
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::TurnFinished { .. }))
            );
            let step = adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"turn","status":status,"items":[]}}})).unwrap();
            let outcome = step.events.iter().find_map(|event| match event {
                Event::TurnFinished { key, outcome } => Some((key.as_deref(), *outcome)),
                _ => None,
            });
            assert_eq!(
                outcome,
                expected.map(|kind| (Some("codex:thread-exact:turn"), kind))
            );
        }
    }

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
                unset_environment: Default::default(),
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

    #[test]
    fn native_queue_is_confirmed_paginated_and_refreshed_after_consumption() {
        let mut adapter = adapter();
        let prompt = adapter
            .command(Command::Prompt {
                text: "next".into(),
                attachments: Vec::new(),
                mode: PromptMode::Queue,
            })
            .unwrap();
        assert!(
            !prompt
                .events
                .iter()
                .any(|event| matches!(event, Event::QueueChanged(_)))
        );
        let added = adapter.receive(json!({"id":prompt.writes[0]["id"], "result":{"queuedSubmission":{"id":"q1","input":[{"type":"text","text":"next"}]}}})).unwrap();
        assert_eq!(added.writes[0]["method"], "thread/queue/list");
        let page = adapter.receive(json!({"id":added.writes[0]["id"],"result":{"data":[{"id":"q1","input":[{"type":"text","text":"next"}]}],"nextCursor":"page2"}})).unwrap();
        assert!(
            !page
                .events
                .iter()
                .any(|event| matches!(event, Event::QueueChanged(_)))
        );
        assert_eq!(page.writes[0]["params"]["cursor"], "page2");
        let last = adapter.receive(json!({"id":page.writes[0]["id"],"result":{"data":[{"id":"q2","input":[{"type":"localImage","path":"/fixture/image.png"}]}],"nextCursor":null}})).unwrap();
        assert!(last.events.iter().any(|event| matches!(event, Event::QueueChanged(messages) if messages.len() == 2 && messages[0].text == "next" && messages[0].editable && messages[1].text.is_empty() && messages[1].editable && messages[1].attachments == [Attachment { path: "/fixture/image.png".into() }])));
        let changed = adapter
            .receive(json!({"method":"thread/queue/changed","params":{"threadId":"thread-exact"}}))
            .unwrap();
        let empty = adapter
            .receive(json!({"id":changed.writes[0]["id"],"result":{"data":[],"nextCursor":null}}))
            .unwrap();
        assert!(
            empty
                .events
                .iter()
                .any(|event| matches!(event, Event::QueueChanged(messages) if messages.is_empty()))
        );
    }

    #[test]
    fn native_queue_rejection_and_stale_pages_never_publish_false_previews() {
        let mut adapter = adapter();
        let prompt = adapter
            .command(Command::Prompt {
                text: "rejected".into(),
                attachments: Vec::new(),
                mode: PromptMode::Queue,
            })
            .unwrap();
        let rejected = adapter
            .receive(
                json!({"id":prompt.writes[0]["id"],"error":{"code":-1,"message":"queue rejected"}}),
            )
            .unwrap();
        assert!(
            rejected
                .events
                .iter()
                .any(|event| matches!(event, Event::Error { fatal: false, .. }))
        );
        assert!(
            !rejected
                .events
                .iter()
                .any(|event| matches!(event, Event::QueueChanged(_)))
        );
        let first = adapter
            .receive(json!({"method":"thread/queue/changed","params":{"threadId":"thread-exact"}}))
            .unwrap();
        let invalidated = adapter
            .receive(json!({"method":"thread/queue/changed","params":{"threadId":"thread-exact"}}))
            .unwrap();
        assert!(invalidated.writes.is_empty());
        let stale = adapter.receive(json!({"id":first.writes[0]["id"],"result":{"data":[{"id":"old","input":[{"type":"text","text":"old"}]}],"nextCursor":null}})).unwrap();
        assert!(
            !stale
                .events
                .iter()
                .any(|event| matches!(event, Event::QueueChanged(_)))
        );
        assert_eq!(stale.writes[0]["method"], "thread/queue/list");
    }

    #[test]
    fn fast_mode_acknowledgements_preserve_model_and_effort_and_reject_false_state() {
        let mut adapter = adapter();
        adapter.fast_models.insert("model".into());
        adapter.reasoning = Some("future-effort".into());
        let request = adapter
            .command(Command::SetFastMode { enabled: true })
            .unwrap();
        assert_eq!(request.writes[0]["method"], "thread/settings/update");
        assert_eq!(
            request.writes[0]["params"],
            json!({"threadId":"thread-exact","serviceTier":"fast"})
        );
        assert!(request.events.is_empty());
        assert!(
            adapter
                .command(Command::SetFastMode { enabled: true })
                .is_err()
        );
        let refused = adapter.receive(json!({"id":request.writes[0]["id"],"error":{"code":-1,"message":"fast unavailable"}})).unwrap();
        assert!(
            refused
                .events
                .iter()
                .any(|event| matches!(event, Event::Error { fatal: false, .. }))
        );
        assert!(adapter.service_tier.is_none());
        let request = adapter
            .command(Command::SetFastMode { enabled: true })
            .unwrap();
        let accepted = adapter
            .receive(json!({"id":request.writes[0]["id"],"result":{}}))
            .unwrap();
        assert!(
            accepted
                .events
                .iter()
                .any(|event| matches!(event, Event::FastMode(value) if value.enabled()))
        );
        assert_eq!(adapter.selected.as_deref(), Some("model"));
        assert_eq!(adapter.reasoning.as_deref(), Some("future-effort"));
        let request = adapter
            .command(Command::SetFastMode { enabled: false })
            .unwrap();
        assert!(request.writes[0]["params"]["serviceTier"].is_null());
        adapter
            .receive(json!({"id":request.writes[0]["id"],"result":{}}))
            .unwrap();
        assert!(adapter.service_tier.is_none());
    }

    #[test]
    fn fast_mode_applies_staged_model_and_effort_with_the_tier() {
        let mut adapter = adapter();
        adapter.effective_model = Some("previous-model".into());
        adapter.fast_models.insert("model".into());
        adapter
            .command(Command::SetModel {
                model: "model".into(),
                reasoning: Some("future-effort".into()),
            })
            .unwrap();
        let request = adapter
            .command(Command::SetFastMode { enabled: true })
            .unwrap();
        assert_eq!(
            request.writes[0]["params"],
            json!({"threadId":"thread-exact","serviceTier":"fast","model":"model","effort":"future-effort"})
        );
        adapter
            .receive(json!({"id":request.writes[0]["id"],"result":{}}))
            .unwrap();
        assert_eq!(adapter.effective_model.as_deref(), Some("model"));
        assert_eq!(adapter.reasoning.as_deref(), Some("future-effort"));
    }

    #[test]
    fn fast_mode_settings_notifications_win_over_delayed_acknowledgements() {
        let mut adapter = adapter();
        adapter.fast_models.insert("model".into());
        let request = adapter
            .command(Command::SetFastMode { enabled: true })
            .unwrap();
        adapter.receive(json!({"method":"thread/settings/updated","params":{"threadId":"thread-exact","threadSettings":{"model":"model","effort":null,"serviceTier":null}}})).unwrap();
        let reply = adapter
            .receive(json!({"id":request.writes[0]["id"],"result":{}}))
            .unwrap();
        assert!(adapter.service_tier.is_none());
        assert!(
            !reply
                .events
                .iter()
                .any(|event| matches!(event, Event::FastMode(value) if value.enabled()))
        );
    }

    #[test]
    fn fast_mode_is_advertised_only_by_selected_model_service_tiers() {
        let mut adapter = adapter();
        assert!(
            adapter
                .command(Command::SetFastMode { enabled: true })
                .is_err()
        );
        for field in [
            json!({"serviceTiers":[{"id":"fast","name":"Fast","description":"Faster generation"}]}),
            json!({"additionalSpeedTiers":["fast"]}),
        ] {
            let mut value =
                json!({"model":"model","displayName":"Model","supportedReasoningEfforts":[]});
            value
                .as_object_mut()
                .unwrap()
                .extend(field.as_object().unwrap().clone());
            let mut step = Step::default();
            adapter
                .call(&mut step, "model/list", json!({}), Pending::Models)
                .unwrap();
            adapter
                .receive(
                    json!({"id":step.writes[0]["id"],"result":{"data":[value],"nextCursor":null}}),
                )
                .unwrap();
            assert!(adapter.capabilities().fast_mode);
        }
        adapter.direct_input = false;
        assert!(!adapter.capabilities().fast_mode);
        assert!(
            adapter
                .command(Command::SetFastMode { enabled: true })
                .is_err()
        );
    }

    #[test]
    fn codex_cost_estimate_keeps_active_turn_tariff_through_model_and_fast_changes() {
        let mut adapter = adapter();
        adapter.selected = Some("gpt-6.1-sol".into());
        adapter.effective_model = adapter.selected.clone();
        let started = |id| json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":id,"items":[],"status":"inProgress"}}});
        let usage = |id, input, output, last_input, last_output| json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-exact","turnId":id,"tokenUsage":{"total":{"inputTokens":input,"cachedInputTokens":0,"outputTokens":output},"last":{"inputTokens":last_input,"cachedInputTokens":0,"outputTokens":last_output}}}});
        let estimate = |step: Step| {
            step.events
                .into_iter()
                .find_map(|event| match event {
                    Event::Usage(usage) => usage.cost_estimate.map(|estimate| estimate.microusd),
                    _ => None,
                })
                .unwrap()
        };
        adapter.receive(started("first")).unwrap();
        assert_eq!(
            estimate(
                adapter
                    .receive(usage("first", 1000, 100, 1000, 100))
                    .unwrap()
            ),
            3000
        );
        adapter.selected = Some("unpriced-staged-selection".into());
        adapter.service_tier = Some("fast".into());
        assert_eq!(
            estimate(
                adapter
                    .receive(usage("first", 2000, 200, 2000, 200))
                    .unwrap()
            ),
            6000
        );
        adapter.receive(started("second")).unwrap();
        assert_eq!(
            estimate(
                adapter
                    .receive(usage("second", 3000, 300, 1000, 100))
                    .unwrap()
            ),
            12000
        );
        assert_eq!(
            estimate(
                adapter
                    .receive(usage("first", 2000, 200, 2000, 200))
                    .unwrap()
            ),
            12000
        );
    }

    fn adapter() -> Codex {
        Codex {
            server_identity: None,
            retained_identity: None,
            thread: Some("thread-exact".into()),
            start: json!({}),
            alias: None,
            cwd: PathBuf::from("/tmp"),
            expected_home: std::fs::canonicalize("/tmp").unwrap(),
            next_id: 0,
            pending: BTreeMap::new(),
            requests: BTreeMap::new(),
            active_items: BTreeMap::new(),
            output_streams: BTreeMap::new(),
            output_windows: BTreeMap::new(),
            models: vec![Model {
                id: "model".into(),
                label: "Model".into(),
                reasoning: vec!["future-effort".into()],
            }],
            listing: Vec::new(),
            modalities: BTreeMap::new(),
            selected: Some("model".into()),
            reasoning: None,
            selection_dirty: false,
            service_tier: None,
            service_tier_revision: 0,
            fast_models: Default::default(),
            effective_model: None,
            pricing: Default::default(),
            turn_pricing: Default::default(),
            turn: None,
            busy: false,
            lifecycle_revision: 0,
            suggestion_turn: None,
            prediction_turn: None,
            predictions_unsupported: false,
            subagent_status: None,
            subagent_waiting: false,
            subagent_detail: String::new(),
            ready: true,
            attached: true,
            direct_input: true,
            cursor: None,
            history_initialized: false,
            history_loading: false,
            history_refresh: false,
            queue_available: true,
            queue_stale: false,
            queue_listing: Vec::new(),
            queue_cursors: std::collections::BTreeSet::new(),
            skills_stale: false,
            skill_paths: Vec::new(),
            materialized: true,
            execution: None,
            children: BTreeMap::new(),
            branch_points: Vec::new(),
            is_child: false,
            child_buffer: BTreeMap::new(),
        }
    }

    #[test]
    fn retry_notices_are_timeline_items_and_exhausted_retries_are_errors() {
        let mut adapter = adapter();
        let frame = json!({"method":"error","params":{"threadId":"thread-exact","turnId":"turn","willRetry":true,"error":{"message":"Reconnecting... 1/2"}}});
        let first = adapter.receive(frame.clone()).unwrap();
        let second = adapter.receive(frame.clone()).unwrap();
        let [Event::Item(item)] = first.events.as_slice() else {
            panic!("{:?}", first.events)
        };
        assert_eq!(item.role, Role::System);
        assert!(
            matches!(&item.content, ItemContent::Text { text, thinking: false, phase: Phase::Commentary } if text == "Reconnecting... 1/2")
        );
        let [Event::Item(next)] = second.events.as_slice() else {
            panic!("{:?}", second.events)
        };
        assert_ne!(item.id, next.id);
        for will_retry in [json!(false), Value::Null] {
            let mut failed = frame.clone();
            failed["params"]["willRetry"] = will_retry;
            assert!(matches!(
                adapter.receive(failed).unwrap().events.as_slice(),
                [Event::Error { fatal: false, .. }]
            ));
        }
        let mut foreign = frame;
        foreign["params"]["threadId"] = json!("another-thread");
        assert!(adapter.receive(foreign).unwrap().events.is_empty());
    }

    #[test]
    fn child_retry_is_recorded_in_the_child_timeline() {
        let mut parent = adapter();
        let mut child = adapter();
        child.thread = Some("child-thread".into());
        child.is_child = true;
        parent
            .children
            .insert("child-thread".into(), Box::new(child));
        let step = parent.receive(json!({"method":"error","params":{"threadId":"child-thread","turnId":"child-turn","willRetry":true,"error":{"message":"Reconnecting... 1/2"}}})).unwrap();
        assert!(
            matches!(step.events.as_slice(), [Event::SubagentItem { id, item: Item { role: Role::System, .. } }] if id == "child-thread")
        );
    }

    #[test]
    fn provider_warnings_are_readable_advisory_timeline_items() {
        for (method, params, expected) in [
            (
                "warning",
                json!({"message":"Warning", "threadId":"thread-exact"}),
                "Warning",
            ),
            (
                "guardianWarning",
                json!({"message":"Guardian warning", "threadId":"thread-exact"}),
                "Guardian warning",
            ),
            (
                "configWarning",
                json!({"summary":"Config warning", "details":"Fix this setting", "path":"/config.toml"}),
                "Config warning\n\nFix this setting\n\n/config.toml",
            ),
            (
                "deprecationNotice",
                json!({"summary":"Deprecated", "details":"Use the new option"}),
                "Deprecated\n\nUse the new option",
            ),
        ] {
            let step = adapter()
                .receive(json!({"method":method,"params":params}))
                .unwrap();
            assert!(
                matches!(step.events.as_slice(), [Event::Item(Item { role: Role::System, content: ItemContent::Text {text, ..}, .. })] if text == expected),
                "{method}: {:?}",
                step.events
            );
        }
    }

    #[test]
    fn history_loading_lasts_until_the_provider_response_and_refresh_coalesces() {
        let mut adapter = adapter();
        adapter.cursor = Some("older".into());
        let request = adapter.command(Command::LoadEarlier).unwrap();
        assert!(request.events.contains(&Event::HistoryLoading(true)));
        assert!(adapter.history_loading);
        assert!(adapter.command(Command::LoadEarlier).is_err());

        // Returning to cached latest must succeed while the older page is
        // still in flight, without issuing a competing history request.
        let refresh = adapter.command(Command::Refresh).unwrap();
        assert!(refresh.writes.is_empty());
        assert!(adapter.command(Command::Refresh).unwrap().writes.is_empty());
        assert!(adapter.history_loading);
        assert!(!refresh.events.contains(&Event::HistoryLoading(false)));

        let answer = adapter
            .receive(json!({"id":request.writes[0]["id"],"result":{"data":[],"nextCursor":null}}))
            .unwrap();
        assert!(
            adapter.history_loading,
            "the requested latest reread follows the older page"
        );
        assert!(matches!(answer.events[0], Event::History { .. }));
        assert_eq!(answer.writes.len(), 2);
        assert_eq!(answer.writes[0]["method"], "thread/read");
        assert_eq!(answer.writes[1]["method"], "thread/turns/list");
        assert!(answer.writes[1]["params"]["cursor"].is_null());
        assert_eq!(answer.events.last(), Some(&Event::LatestHistoryStarted));
        let latest = adapter
            .receive(json!({"id":answer.writes[1]["id"],"result":{"data":[],"nextCursor":null}}))
            .unwrap();
        assert!(!adapter.history_loading);
        assert!(matches!(latest.events[0], Event::LatestHistory { .. }));
        assert_eq!(latest.events.last(), Some(&Event::HistoryLoading(false)));
    }

    #[test]
    fn failed_history_request_releases_loading_and_can_be_retried() {
        let mut adapter = adapter();
        adapter.cursor = Some("older".into());
        let request = adapter.command(Command::LoadEarlier).unwrap();
        let failed = adapter
            .receive(
                json!({"id":request.writes[0]["id"],"error":{"code":-1,"message":"try later"}}),
            )
            .unwrap();
        assert!(!adapter.history_loading);
        assert_eq!(failed.events[0], Event::HistoryLoading(false));
        assert!(matches!(failed.events[1], Event::HistoryError(_)));
        assert!(adapter.command(Command::LoadEarlier).is_ok());
    }

    #[test]
    fn failed_older_history_still_runs_one_coalesced_latest_refresh() {
        let mut adapter = adapter();
        adapter.cursor = Some("older".into());
        let request = adapter.command(Command::LoadEarlier).unwrap();
        for _ in 0..10 {
            assert!(adapter.command(Command::Refresh).unwrap().writes.is_empty());
        }
        let failed = adapter
            .receive(
                json!({"id":request.writes[0]["id"],"error":{"message":"older page unavailable"}}),
            )
            .unwrap();
        assert!(adapter.history_loading);
        assert_eq!(failed.writes.len(), 2);
        assert_eq!(failed.writes[1]["method"], "thread/turns/list");
        assert!(failed.writes[1]["params"]["cursor"].is_null());
        let latest = adapter
            .receive(json!({"id":failed.writes[1]["id"],"result":{"data":[],"nextCursor":null}}))
            .unwrap();
        assert!(!adapter.history_loading);
        assert_eq!(latest.events.last(), Some(&Event::HistoryLoading(false)));
    }

    #[test]
    fn latest_history_refresh_keeps_the_furthest_older_cursor_including_exhaustion() {
        let mut adapter = adapter();
        let mut step = Step::default();
        for (cursor, earlier) in [
            (Some("100"), false),
            (Some("80"), true),
            (Some("60"), true),
            (Some("100"), false),
        ] {
            adapter
                .page(&json!({"data":[],"nextCursor":cursor}), &mut step, earlier)
                .unwrap();
        }
        let request = adapter.command(Command::LoadEarlier).unwrap();
        assert_eq!(request.writes[0]["params"]["cursor"], "60");
        adapter
            .receive(json!({"id":request.writes[0]["id"],"result":{"data":[],"nextCursor":null}}))
            .unwrap();
        adapter
            .page(&json!({"data":[],"nextCursor":"100"}), &mut step, false)
            .unwrap();
        assert!(adapter.cursor.is_none());
        assert!(adapter.command(Command::LoadEarlier).is_err());
    }

    #[test]
    fn malformed_history_page_preserves_initialization_and_the_retry_cursor() {
        let mut adapter = adapter();
        let mut step = Step::default();
        let malformed = json!({"data":[{"id":"turn","status":"completed","itemsView":"compact","items":[]}],"nextCursor":"lost"});
        assert!(adapter.page(&malformed, &mut step, false).is_err());
        assert!(!adapter.history_initialized);
        adapter
            .page(&json!({"data":[],"nextCursor":"100"}), &mut step, false)
            .unwrap();
        assert_eq!(adapter.cursor.as_deref(), Some("100"));
        assert!(adapter.page(&malformed, &mut step, true).is_err());
        assert_eq!(adapter.cursor.as_deref(), Some("100"));
    }

    #[test]
    fn failed_initial_history_request_can_initialize_its_cursor_on_retry() {
        let mut adapter = adapter();
        let request = adapter.command(Command::Refresh).unwrap();
        adapter
            .receive(json!({"id":request.writes[1]["id"],"error":{"message":"try later"}}))
            .unwrap();
        assert!(!adapter.history_initialized);
        assert!(adapter.cursor.is_none());
        let retry = adapter.command(Command::Refresh).unwrap();
        adapter
            .receive(json!({"id":retry.writes[1]["id"],"result":{"data":[],"nextCursor":"100"}}))
            .unwrap();
        assert!(adapter.history_initialized);
        assert_eq!(adapter.cursor.as_deref(), Some("100"));
    }

    #[test]
    fn reattachment_requests_subagent_sources_explicitly() {
        let mut parent = adapter();
        parent
            .pending
            .insert(json!("attach").to_string(), Pending::Attach);
        let attached = parent.receive(json!({"id":"attach","result":{"thread":{"id":"thread-exact","status":{"type":"idle"}},"initialTurnsPage":{"data":[],"nextCursor":null}}})).unwrap();
        let list = attached
            .writes
            .iter()
            .find(|frame| frame["method"] == "thread/list")
            .unwrap();
        assert_eq!(
            list["params"]["sourceKinds"],
            json!(["subAgentThreadSpawn"])
        );
        assert_eq!(list["params"]["ancestorThreadId"], "thread-exact");
    }

    #[test]
    fn proven_child_start_discovers_early_streams_and_hydration_failure_keeps_them() {
        let mut parent = adapter();
        let started = parent.receive(json!({"method":"thread/started","params":{"thread":{"id":"child", "parentThreadId":"thread-exact", "agentNickname":"Scout", "status":{"type":"active"}}}})).unwrap();
        assert_eq!(started.writes[0]["method"], "thread/resume");
        assert!(parent.children.contains_key("child"));
        let live = parent.receive(json!({"method":"item/agentMessage/delta","params":{"threadId":"child","turnId":"turn","itemId":"answer","delta":"Early streamed text"}})).unwrap();
        assert!(live.events.is_empty());
        let failed = parent
            .receive(
                json!({"id":started.writes[0]["id"],"error":{"message":"History read failed"}}),
            )
            .unwrap();
        assert!(parent.children.contains_key("child"));
        assert!(!parent.child_buffer.contains_key("child"));
        assert!(failed.events.iter().any(|event| matches!(event, Event::SubagentTextDelta { id, delta, .. } if id == "child" && delta == "Early streamed text")));
        assert!(!failed.events.iter().any(|event| matches!(
            event,
            Event::Subagent {
                status: Some(ToolStatus::Failed),
                ..
            }
        )));
        let foreign = parent.receive(json!({"method":"thread/started","params":{"thread":{"id":"foreign","parentThreadId":"another-root"}}})).unwrap();
        assert!(foreign.events.is_empty());
        assert!(!parent.children.contains_key("foreign"));
    }

    #[test]
    fn child_discovery_cannot_replay_active_metadata_over_idle_hydration() {
        let mut parent = adapter();
        let started = parent.receive(json!({"method":"thread/started","params":{"thread":{"id":"child","parentThreadId":"thread-exact","agentNickname":"Scout","status":{"type":"active"}}}})).unwrap();
        let hydrated = parent.receive(json!({"id":started.writes[0]["id"],"result":{"thread":{"id":"child","status":{"type":"idle"}},"initialTurnsPage":{"data":[],"nextCursor":null}}})).unwrap();
        assert!(!parent.children["child"].busy);
        assert!(hydrated.events.iter().any(|event| matches!(
            event,
            Event::Subagent {
                status: Some(ToolStatus::Succeeded),
                ..
            }
        )));
        let mut sparse = Step::default();
        parent
            .discover_child("child", &json!({}), &mut sparse)
            .unwrap();
        assert!(sparse.events.iter().any(
            |event| matches!(event, Event::Subagent {kind, status:None, ..} if kind.is_empty())
        ));
    }

    #[test]
    fn native_prompt_late_ack_does_not_claim_a_new_active_turn_without_an_id() {
        let mut adapter = adapter();
        let sent = adapter
            .command(Command::Prompt {
                text: "Prompt".into(),
                attachments: Vec::new(),
                mode: PromptMode::Send,
            })
            .unwrap();
        adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"old","status":"completed","items":[]}}})).unwrap();
        adapter.receive(json!({"method":"thread/status/changed","params":{"threadId":"thread-exact","status":{"type":"active"}}})).unwrap();
        adapter.receive(json!({"id":sent.writes[0]["id"],"result":{"turn":{"id":"old","status":"inProgress","items":[]}}})).unwrap();
        assert!(adapter.busy);
        assert_eq!(adapter.turn, None);
    }

    #[test]
    fn child_terminal_outcomes_survive_idle_and_closure_and_new_turn_restarts_work() {
        for (outcome, expected) in [
            ("failed", ToolStatus::Failed),
            ("interrupted", ToolStatus::Stopped),
            ("completed", ToolStatus::Succeeded),
        ] {
            let mut parent = adapter();
            let mut child = adapter();
            child.thread = Some("child".into());
            child.is_child = true;
            parent.children.insert("child".into(), Box::new(child));
            parent.receive(json!({"method":"turn/completed","params":{"threadId":"child","turn":{"id":"t","status":outcome,"items":[]}}})).unwrap();
            for frame in [
                json!({"method":"thread/status/changed","params":{"threadId":"child","status":{"type":"idle"}}}),
                json!({"method":"thread/closed","params":{"threadId":"child"}}),
            ] {
                let observed = parent.receive(frame).unwrap();
                assert!(observed.events.iter().any(|event| matches!(event, Event::Subagent {status:Some(status), ..} if *status == expected)), "{:?}", observed.events);
            }
            let next = parent.receive(json!({"method":"turn/started","params":{"threadId":"child","turn":{"id":"next","items":[]}}})).unwrap();
            assert!(next.events.iter().any(|event| matches!(
                event,
                Event::Subagent {
                    status: Some(ToolStatus::Running),
                    ..
                }
            )));
        }
        let mut parent = adapter();
        let mut child = adapter();
        child.thread = Some("child".into());
        child.is_child = true;
        child.subagent_status = Some(ToolStatus::Running);
        parent.children.insert("child".into(), Box::new(child));
        let closed = parent
            .receive(json!({"method":"thread/closed","params":{"threadId":"child"}}))
            .unwrap();
        assert!(closed.events.iter().any(|event| matches!(
            event,
            Event::Subagent {
                status: Some(ToolStatus::Stopped),
                ..
            }
        )));
    }

    /// A child is a thread of its own: stopping it interrupts its turn alone,
    /// and cancelling the parent's turn names only the parent's.
    #[test]
    fn stopping_a_child_interrupts_only_its_own_turn() {
        let mut parent = adapter();
        parent.thread = Some("parent".into());
        parent.turn = Some("parent-turn".into());
        parent.ready = true;
        for (id, turn) in [
            ("busy-child", Some("child-turn")),
            ("sibling", Some("sibling-turn")),
            ("idle-child", None),
        ] {
            let mut child = adapter();
            child.thread = Some(id.into());
            child.is_child = true;
            child.turn = turn.map(str::to_owned);
            parent.children.insert(id.into(), Box::new(child));
        }
        assert!(parent.capabilities().stop_subagent);
        assert!(!parent.capabilities().interrupt_stops_background);
        let stop = parent
            .command(Command::StopSubagent {
                id: "busy-child".into(),
            })
            .unwrap();
        assert_eq!(stop.writes.len(), 1);
        assert_eq!(stop.writes[0]["method"], "turn/interrupt");
        assert_eq!(
            stop.writes[0]["params"],
            json!({"threadId":"busy-child","turnId":"child-turn"})
        );
        let refused = parent
            .receive(json!({"id":stop.writes[0]["id"],"error":{"code":-32600,"message":"no active turn to interrupt"}}))
            .unwrap();
        assert!(refused.events.iter().any(|event| matches!(
            event,
            Event::Error { message, fatal: false } if message.starts_with("Codex could not stop the subagent")
        )));
        assert!(
            parent
                .command(Command::StopSubagent {
                    id: "idle-child".into()
                })
                .is_err()
        );
        assert!(
            parent
                .command(Command::StopSubagent {
                    id: "unknown".into()
                })
                .is_err()
        );
        let stop = parent
            .command(Command::StopSubagent {
                id: "busy-child".into(),
            })
            .unwrap();
        let acknowledged = parent
            .receive(json!({"id":stop.writes[0]["id"],"result":{}}))
            .unwrap();
        assert!(acknowledged.events.is_empty());
        assert_eq!(parent.turn.as_deref(), Some("parent-turn"));
        assert_eq!(
            parent.children["sibling"].turn.as_deref(),
            Some("sibling-turn")
        );
        assert_eq!(
            parent.children["busy-child"].turn.as_deref(),
            Some("child-turn")
        );
        let cancel = parent.command(Command::Interrupt).unwrap();
        assert_eq!(
            cancel.writes[0]["params"],
            json!({"threadId":"parent","turnId":"parent-turn"})
        );
    }

    #[test]
    fn native_prompt_late_ack_and_old_completion_cannot_reactivate_or_stop_a_new_turn() {
        for newer in [false, true] {
            let mut adapter = adapter();
            let sent = adapter
                .command(Command::Prompt {
                    text: "Prompt".into(),
                    attachments: Vec::new(),
                    mode: PromptMode::Send,
                })
                .unwrap();
            let start = |id| json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":id,"status":"inProgress","items":[]}}});
            let complete = json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"old","status":"completed","items":[]}}});
            adapter.receive(start("old")).unwrap();
            adapter.receive(complete.clone()).unwrap();
            if newer {
                adapter.receive(start("new")).unwrap();
            }
            let ack = adapter.receive(json!({"id":sent.writes[0]["id"],"result":{"turn":{"id":"old","status":"inProgress","items":[]}}})).unwrap();
            assert_eq!(adapter.busy, newer);
            assert_eq!(adapter.turn.as_deref(), newer.then_some("new"));
            assert!(
                !ack.events
                    .iter()
                    .any(|event| matches!(event, Event::Status { busy: true, .. }))
            );
            if newer {
                adapter.receive(json!({"method":"item/started","params":{"threadId":"thread-exact","turnId":"new","item":{"id":"cmd","type":"commandExecution","command":"echo hi","cwd":"/tmp","status":"inProgress","commandActions":[]}}})).unwrap();
                let old = adapter.receive(complete).unwrap();
                assert!(
                    !old.events
                        .iter()
                        .any(|event| matches!(event, Event::TurnSettled))
                );
                assert!(adapter.busy);
                assert_eq!(adapter.turn.as_deref(), Some("new"));
                assert!(!adapter.active_items.is_empty());
                let current = adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"new","status":"completed","items":[]}}})).unwrap();
                assert!(
                    current
                        .events
                        .iter()
                        .any(|event| matches!(event, Event::TurnSettled))
                );
            }
        }
    }

    #[test]
    fn child_hydration_buffers_live_output_and_routes_unique_rpc_ids() {
        let mut adapter = adapter();
        let mut discovered = Step::default();
        adapter
            .discover_child("child-exact", &json!({"status":"running"}), &mut discovered)
            .unwrap();
        assert_eq!(
            discovered.writes[0]["params"]["initialTurnsPage"]["itemsView"],
            "full"
        );
        let live = adapter.receive(json!({"method":"item/agentMessage/delta","params":{"threadId":"child-exact","turnId":"turn","itemId":"answer","delta":" tail"}})).unwrap();
        assert!(live.events.is_empty());
        let hydrated = adapter.receive(json!({"id":discovered.writes[0]["id"],"result":{"thread":{"id":"child-exact","status":{"type":"active"}},"initialTurnsPage":{"data":[{"id":"turn","status":"inProgress","items":[{"id":"answer","type":"agentMessage","text":"head"}]}],"nextCursor":null}}})).unwrap();
        assert!(hydrated.events.iter().any(|event| matches!(event, Event::SubagentTextDelta { id, delta, .. } if id == "child-exact" && delta == " tail")));
        assert!(
            !hydrated
                .events
                .iter()
                .any(|event| matches!(event, Event::Status { .. }))
        );
        let invalidated = adapter
            .receive(json!({"method":"skills/changed","params":{"threadId":"child-exact"}}))
            .unwrap();
        assert_eq!(invalidated.writes.len(), 1);
        assert_ne!(invalidated.writes[0]["id"], discovered.writes[0]["id"]);
        let replied = adapter
            .receive(json!({"id":invalidated.writes[0]["id"],"result":{"data":[]}}))
            .unwrap();
        assert!(replied.events.is_empty());
        let completed = adapter.receive(json!({"method":"turn/completed","params":{"threadId":"child-exact","turn":{"id":"turn","status":"completed","items":[]}}})).unwrap();
        assert!(completed.events.iter().any(|event| matches!(event, Event::Subagent { id, status:Some(ToolStatus::Succeeded), .. } if id == "child-exact")));
        let foreign = adapter.receive(json!({"method":"turn/started","params":{"threadId":"unrelated","turn":{"id":"bad","items":[]}}})).unwrap();
        assert!(foreign.events.is_empty());
    }

    #[test]
    fn a_queue_removal_is_answered_as_removed_or_refused() {
        for (answer, removed) in [
            (json!({"result":{}}), true),
            (
                json!({"error":{"code":-32000,"message":"already started"}}),
                false,
            ),
        ] {
            let mut adapter = adapter();
            let request = adapter
                .command(Command::RemoveQueued { id: "q1".into() })
                .unwrap();
            let mut frame = answer;
            frame["id"] = request.writes[0]["id"].clone();
            let step = adapter.receive(frame).unwrap();
            assert!(step.events.iter().any(|event| matches!(
                event,
                Event::QueueRemoval(removal) if removal.id == "q1" && removal.removed == removed
            )));
            // Either way the listing is read again rather than assumed.
            assert!(
                step.writes
                    .iter()
                    .any(|write| write["method"] == "thread/queue/list")
            );
        }
    }

    #[test]
    fn queue_mutations_and_forks_use_exact_provider_identifiers() {
        let mut adapter = adapter();
        for (command, method) in [
            (
                Command::RemoveQueued {
                    id: "queue-exact".into(),
                },
                "thread/queue/delete",
            ),
            (
                Command::StartQueued {
                    id: "queue-exact".into(),
                },
                "thread/queue/start",
            ),
        ] {
            let request = adapter.command(command).unwrap();
            assert_eq!(request.writes[0]["method"], method);
            assert_eq!(request.writes[0]["params"]["threadId"], "thread-exact");
            assert_eq!(
                request.writes[0]["params"]["queuedSubmissionId"],
                "queue-exact"
            );
        }
        adapter.branch_points.push(chat::BranchPoint {
            id: "completed-turn".into(),
            label: "Prompt".into(),
        });
        assert!(
            adapter
                .command(Command::Branch {
                    id: Some("unknown-turn".into())
                })
                .is_err()
        );
        adapter.busy = true;
        assert!(adapter.command(Command::Branch { id: None }).is_err());
        adapter.busy = false;
        let fork = adapter
            .command(Command::Branch {
                id: Some("completed-turn".into()),
            })
            .unwrap();
        assert_eq!(fork.writes[0]["params"]["lastTurnId"], "completed-turn");
        let fork_id = "f0e49307-af96-4edd-b23f-60b185fe4fea";
        let response = adapter
            .receive(json!({"id":fork.writes[0]["id"],"result":{"thread":{"id":fork_id}}}))
            .unwrap();
        assert!(response.events.contains(&Event::Forked(fork_id.into())));
        assert_eq!(adapter.thread.as_deref(), Some("thread-exact"));
        assert_eq!(response.writes[0]["method"], "thread/unsubscribe");
    }

    #[test]
    fn child_listing_recovers_newest_first_descendants_without_trusting_foreign_threads() {
        let mut adapter = adapter();
        let mut request = Step::default();
        adapter
            .call(&mut request, "thread/list", json!({}), Pending::ChildList)
            .unwrap();
        adapter
            .receive(json!({"id":request.writes[0]["id"],"result":{"data":[
                {"id":"grandchild","parentThreadId":"child","status":{"type":"idle"}},
                {"id":"foreign","parentThreadId":"another-root"},
                {"id":"child","parentThreadId":"thread-exact","status":{"type":"idle"}}
            ]}}))
            .unwrap();
        assert!(adapter.children.contains_key("child"));
        assert!(adapter.children.contains_key("grandchild"));
        assert!(!adapter.children.contains_key("foreign"));
    }

    #[test]
    fn child_history_recovers_failed_and_interrupted_outcomes_without_rewinding_live_work() {
        for (outcome, expected) in [
            ("failed", ToolStatus::Failed),
            ("interrupted", ToolStatus::Stopped),
            ("completed", ToolStatus::Succeeded),
        ] {
            let mut parent = adapter();
            let mut discovered = Step::default();
            parent
                .discover_child("child", &json!({"status":"running"}), &mut discovered)
                .unwrap();
            let restored = parent.receive(json!({"id":discovered.writes[0]["id"],"result":{"thread":{"id":"child","status":{"type":"idle"}},"initialTurnsPage":{"data":[{"id":"latest","status":outcome,"items":[]},{"id":"older","status":"completed","items":[]}],"nextCursor":null}}})).unwrap();
            assert_eq!(parent.children["child"].subagent_status, Some(expected));
            assert!(restored.events.iter().any(|event| matches!(event, Event::Subagent { id, status:Some(status), .. } if id == "child" && *status == expected)));
            let child = parent.children.get_mut("child").unwrap();
            let mut page = Step::default();
            child.page(&json!({"data":[{"id":"oldest","status":"completed","items":[]}],"nextCursor":null}), &mut page, true).unwrap();
            assert_eq!(child.subagent_status, Some(expected));
            child.observe_thread_status(&json!({"type":"active"}));
            child.page(&json!({"data":[{"id":"latest","status":outcome,"items":[]}],"nextCursor":null}), &mut page, false).unwrap();
            assert_eq!(child.subagent_status, Some(ToolStatus::Running));
            assert!(child.busy);
        }
    }

    #[test]
    fn paging_historical_collaboration_cannot_restart_a_finished_child() {
        let mut parent = adapter();
        let mut child = adapter();
        child.is_child = true;
        child.thread = Some("child".into());
        child.subagent_status = Some(ToolStatus::Failed);
        child.selected = Some("gpt-new".into());
        parent.children.insert("child".into(), Box::new(child));
        let mut step = Step::default();
        parent.page(&json!({"data":[{"id":"older","status":"completed","items":[{"id":"spawn","type":"collabAgentToolCall","tool":"spawnAgent","status":"completed","senderThreadId":"thread-exact","receiverThreadIds":["child"],"agentsStates":{"child":{"status":"running","agentNickname":"Scout","model":"gpt-old"}}}]}],"nextCursor":null}), &mut step, true).unwrap();
        assert_eq!(
            parent.children["child"].subagent_status,
            Some(ToolStatus::Failed)
        );
        assert_eq!(
            parent.children["child"].selected.as_deref(),
            Some("gpt-new")
        );
        assert!(step.events.iter().any(|event| matches!(event, Event::Subagent { id, kind, status:None, model:None, .. } if id == "child" && kind == "Scout")));
        assert!(!step.events.iter().any(|event| matches!(
            event,
            Event::Subagent {
                status: Some(ToolStatus::Running),
                ..
            }
        )));
    }

    #[test]
    fn metadata_refresh_preserves_the_model_selected_for_the_next_prompt() {
        let mut adapter = adapter();
        adapter.models.push(Model {
            id: "gpt-next".into(),
            label: "Next model".into(),
            reasoning: Vec::new(),
        });
        adapter
            .command(Command::SetModel {
                model: "gpt-next".into(),
                reasoning: None,
            })
            .unwrap();
        adapter.receive(json!({"method":"thread/started","params":{"thread":{"id":"thread-exact","model":"gpt-prior","canAcceptDirectInput":true,"status":{"type":"idle"}}}})).unwrap();
        assert_eq!(adapter.selected.as_deref(), Some("gpt-next"));
        let prompt = adapter
            .command(Command::Prompt {
                text: "Next prompt".into(),
                attachments: Vec::new(),
                mode: PromptMode::Send,
            })
            .unwrap();
        assert_eq!(prompt.writes[0]["method"], "turn/start");
        assert_eq!(prompt.writes[0]["params"]["model"], "gpt-next");
    }

    #[test]
    fn child_model_and_waiting_state_follow_its_own_thread_lifecycle() {
        let mut parent = adapter();
        let mut discovered = Step::default();
        parent
            .discover_child("child", &json!({}), &mut discovered)
            .unwrap();
        assert!(
            discovered
                .events
                .iter()
                .any(|event| matches!(event, Event::Subagent { model: None, .. }))
        );
        let attached = parent.receive(json!({"id":discovered.writes[0]["id"],"result":{"model":"gpt-child","thread":{"id":"child","status":{"type":"active","activeFlags":["waitingOnApproval"]}},"initialTurnsPage":{"data":[],"nextCursor":null}}})).unwrap();
        assert!(attached.events.iter().any(|event| matches!(event, Event::Subagent { model:Some(model), waiting:Some(true), detail:Some(detail), .. } if model == "gpt-child" && detail == "Waiting for your approval")));
        let answered = parent.receive(json!({"method":"thread/status/changed","params":{"threadId":"child","status":{"type":"active","activeFlags":[]}}})).unwrap();
        assert!(answered.events.iter().any(|event| matches!(event, Event::Subagent { model:Some(model), waiting:Some(false), detail:Some(detail), status:Some(ToolStatus::Running), .. } if model == "gpt-child" && detail.is_empty())));
        parent.receive(json!({"method":"thread/status/changed","params":{"threadId":"child","status":{"type":"active","activeFlags":["waitingOnUserInput"]}}})).unwrap();
        let finished = parent.receive(json!({"method":"turn/completed","params":{"threadId":"child","turn":{"id":"latest","status":"completed","items":[]}}})).unwrap();
        assert!(finished.events.iter().any(|event| matches!(
            event,
            Event::Subagent {
                waiting: Some(false),
                status: Some(ToolStatus::Succeeded),
                ..
            }
        )));
        assert_eq!(parent.selected.as_deref(), Some("model"));
    }

    #[test]
    fn child_legacy_embedded_history_retains_the_latest_turn_outcome() {
        let mut parent = adapter();
        let mut discovered = Step::default();
        parent
            .discover_child("child", &json!({}), &mut discovered)
            .unwrap();
        parent.receive(json!({"id":discovered.writes[0]["id"],"result":{"thread":{"id":"child","status":{"type":"idle"},"turns":[{"id":"older","status":"completed","items":[]},{"id":"latest","status":"failed","items":[]}]}}})).unwrap();
        assert_eq!(
            parent.children["child"].subagent_status,
            Some(ToolStatus::Failed)
        );
    }

    #[test]
    fn child_resume_without_embedded_turns_reads_history_and_settles_idle() {
        let mut adapter = adapter();
        let mut discovered = Step::default();
        adapter
            .discover_child("child-exact", &json!({"status":"running"}), &mut discovered)
            .unwrap();
        adapter.receive(json!({"method":"item/agentMessage/delta","params":{"threadId":"child-exact","turnId":"turn","itemId":"answer","delta":" tail"}})).unwrap();
        let resumed = adapter.receive(json!({"id":discovered.writes[0]["id"],"result":{"thread":{"id":"child-exact","status":{"type":"idle"},"turns":[]}}})).unwrap();
        assert!(resumed.events.iter().any(|event| matches!(event, Event::Subagent { id, status:Some(ToolStatus::Succeeded), .. } if id == "child-exact")));
        assert_eq!(resumed.writes[0]["method"], "thread/turns/list");
        assert!(adapter.child_buffer.contains_key("child-exact"));
        let history = adapter.receive(json!({"id":resumed.writes[0]["id"],"result":{"data":[{"id":"turn","status":"completed","items":[{"id":"answer","type":"agentMessage","text":"head"}]}],"nextCursor":null}})).unwrap();
        assert!(
            history.events.iter().any(
                |event| matches!(event, Event::SubagentItem { id, .. } if id == "child-exact")
            )
        );
        assert!(!adapter.child_buffer.contains_key("child-exact"));
    }

    #[test]
    fn child_approvals_resolve_in_parent_and_failed_turns_keep_their_outcome() {
        let mut adapter = adapter();
        let mut discovered = Step::default();
        adapter
            .discover_child("child-exact", &json!({"status":"running"}), &mut discovered)
            .unwrap();
        adapter.receive(json!({"id":discovered.writes[0]["id"],"result":{"thread":{"id":"child-exact","status":{"type":"active"}},"initialTurnsPage":{"data":[],"nextCursor":null}}})).unwrap();
        let approval = |id| json!({"id":id,"method":"item/commandExecution/requestApproval","params":{"threadId":"child-exact","turnId":"turn","itemId":"tool","availableDecisions":["accept","decline"],"command":"echo requested"}});
        let requested = adapter.receive(approval(71)).unwrap();
        assert!(requested.events.iter().any(|event| matches!(event, Event::Request(request) if request.title.starts_with("Child agent"))));
        let resolved = adapter.receive(json!({"method":"serverRequest/resolved","params":{"threadId":"child-exact","requestId":71}})).unwrap();
        assert!(
            resolved
                .events
                .iter()
                .any(|event| matches!(event, Event::RequestResolved { id } if id == "71"))
        );
        assert!(adapter.requests.is_empty());
        let failed = adapter.receive(json!({"method":"turn/completed","params":{"threadId":"child-exact","turn":{"id":"turn","status":"failed","error":{"message":"fixture"},"items":[]}}})).unwrap();
        assert!(failed.events.iter().any(|event| matches!(event, Event::Subagent { id, status:Some(ToolStatus::Failed), .. } if id == "child-exact")));
        adapter.receive(approval(72)).unwrap();
        let closed = adapter
            .receive(json!({"method":"thread/closed","params":{"threadId":"child-exact"}}))
            .unwrap();
        assert!(
            closed
                .events
                .iter()
                .any(|event| matches!(event, Event::RequestResolved { id } if id == "72"))
        );
        assert!(adapter.requests.is_empty());
    }

    #[test]
    fn permission_picker_updates_both_codex_policies() {
        let mut adapter = adapter();
        assert_eq!(
            adapter.capabilities().permission_modes,
            ["readOnly", "workspaceWrite", "yolo"]
        );
        for (mode, approval, sandbox) in [
            ("yolo", "never", "dangerFullAccess"),
            ("workspaceWrite", "on-request", "workspaceWrite"),
            ("readOnly", "on-request", "readOnly"),
        ] {
            let step = adapter
                .command(Command::SetPermissionMode { mode: mode.into() })
                .unwrap();
            assert_eq!(step.writes[0]["method"], "thread/settings/update");
            assert_eq!(step.writes[0]["params"]["approvalPolicy"], approval);
            assert_eq!(step.writes[0]["params"]["sandboxPolicy"]["type"], sandbox);
            let acknowledged = adapter
                .receive(json!({"id":step.writes[0]["id"], "result":{}}))
                .unwrap();
            assert!(
                !acknowledged
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::PermissionMode(_)))
            );
            let confirmed = adapter
                .receive(json!({"method":"thread/settings/updated","params":{
                    "threadId":"thread-exact",
                    "threadSettings": {"approvalPolicy":approval,"sandboxPolicy":{"type":sandbox}}
                }}))
                .unwrap();
            assert!(
                confirmed
                    .events
                    .contains(&Event::PermissionMode(Some(mode.into())))
            );
        }
        let custom = adapter.receive(json!({"method":"thread/settings/updated","params":{
            "threadId":"thread-exact",
            "threadSettings": {"approvalPolicy":"on-failure","sandboxPolicy":{"type":"workspaceWrite"}}
        }})).unwrap();
        assert!(custom.events.contains(&Event::PermissionMode(None)));
        adapter.busy = true;
        assert!(
            adapter
                .command(Command::SetPermissionMode {
                    mode: "yolo".into()
                })
                .is_err()
        );
    }

    #[test]
    fn accepted_run_waits_for_its_exact_turn_even_when_completion_precedes_ack() {
        use crate::agent_execution::ProviderEvent;
        let mut adapter = adapter();
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
        let request = prompt
            .writes
            .iter()
            .find(|frame| frame["method"] == "turn/start")
            .unwrap()["id"]
            .clone();
        let complete = |id: &str, answer: &str| {
            json!({"method":"turn/completed","params":{
                "threadId":"thread-exact","turn":{"id":id,"status":"completed","items":[
                    {"id":format!("{id}-answer"),"type":"agentMessage","text":answer}
                ]}
            }})
        };
        for frame in [
            complete("another-client", "Unrelated answer"),
            complete("owned", "Owned answer"),
        ] {
            let observed = adapter.receive(frame).unwrap();
            assert!(
                !observed
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::Execution { .. }))
            );
        }
        let acknowledged = adapter
            .receive(json!({"id":request,"result":{"turn":{"id":"owned"}}}))
            .unwrap();
        assert!(acknowledged.events.iter().any(|event| matches!(event,
            Event::Execution { run: actual, event: ProviderEvent::Finished { outcome: crate::notifications::Kind::Succeeded } } if *actual == run)));
        let texts = acknowledged
            .events
            .iter()
            .filter_map(|event| match event {
                Event::Item(Item {
                    role: Role::Assistant,
                    content: ItemContent::Text { text, .. },
                    ..
                }) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(texts, ["Owned answer"]);
        assert!(!adapter.busy);
        assert!(adapter.execution.is_none());
    }

    fn account_fixture(name: &str) -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/accounts")
            .join(name);
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    /// Account notifications name no thread and are forwarded as account
    /// reports rather than dropped with the other auxiliary notifications. A
    /// sparse rate-limit update is forwarded as what it names.
    #[test]
    fn account_notifications_are_forwarded_as_reports() {
        let mut adapter = adapter();
        let limits = account_fixture("codex-rate-limits-updated.json");
        let expected = account::codex::parse_update(&limits["params"]).unwrap();
        assert_eq!(expected.windows.len(), 1, "the null window is not cleared");
        assert_eq!(
            adapter.receive(limits).unwrap().events,
            [Event::AccountUsage(LiveReport::CodexLimits(expected))]
        );
        assert_eq!(
            adapter
                .receive(account_fixture("codex-account-updated.json"))
                .unwrap()
                .events,
            [Event::AccountUsage(LiveReport::CodexLogin(
                account::codex::LoginUpdate {
                    chatgpt: true,
                    plan: Some("prolite".into()),
                }
            ))]
        );
        let empty = json!({"method": "account/rateLimits/updated", "params": {"rateLimits": null}});
        assert!(adapter.receive(empty).unwrap().events.is_empty());
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
                unset_environment: Default::default(),
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
                unset_environment: Default::default(),
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

    #[test]
    fn a_new_thread_takes_its_reasoning_effort_as_a_start_override() {
        let directory = tempfile::tempdir().unwrap();
        let launch = |arguments: &[&str]| Launch {
            provider: crate::agent::Provider::Codex,
            session_id: None,
            recipe: crate::model::LaunchRecipe {
                unset_environment: Default::default(),
                program: "codex".into(),
                arguments: arguments.iter().map(|&argument| argument.into()).collect(),
                working_directory: directory.path().into(),
                environment: BTreeMap::from([(
                    "CODEX_HOME".into(),
                    directory.path().to_str().unwrap().into(),
                )]),
            },
        };
        for (arguments, effort) in [
            (&["--config", "model_reasoning_effort=low"][..], "low"),
            (&["-c", "model_reasoning_effort=\"xhigh\""][..], "xhigh"),
        ] {
            let (spec, mut adapter) = create(&launch(arguments)).unwrap();
            // The override is a thread setting, never a server-wide flag.
            assert_eq!(spec.arguments, vec!["app-server"]);
            let initialize = adapter.initialize().unwrap();
            let step = adapter.receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":directory.path()}})).unwrap();
            let start = step
                .writes
                .iter()
                .find(|frame| frame["method"] == "thread/start")
                .unwrap();
            assert_eq!(
                start["params"]["config"],
                json!({REASONING_EFFORT_KEY: effort})
            );
        }
        for arguments in [
            &["--config", "model=\"other\""][..],
            &["-c", "model_reasoning_effort=--yolo"][..],
            &["-c", "model_reasoning_effort_extra=low"][..],
            &["--config"][..],
        ] {
            assert!(create(&launch(arguments)).is_err(), "{arguments:?}");
        }
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

    /// The skills a catalog lists, answered to a fresh adapter.
    fn listed(adapter: &mut Codex, data: Value) -> Vec<Skill> {
        let initialize = adapter.initialize().unwrap();
        let setup = adapter
            .receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":"/tmp"}}))
            .unwrap();
        let list = setup
            .writes
            .iter()
            .find(|frame| frame["method"] == "skills/list")
            .unwrap();
        adapter
            .receive(json!({"id":list["id"],"result":{"data":data}}))
            .unwrap()
            .events
            .into_iter()
            .find_map(|event| match event {
                Event::Skills(skills) => Some(skills),
                _ => None,
            })
            .unwrap()
    }

    /// Codex loads a skill a message names by `$name` only when no other
    /// skill has that name, so two sharing one are each named by a link to
    /// their own file, and every skill a message names goes with it as a
    /// `skill` input carrying its path, as Codex's own composer sends it.
    #[test]
    fn named_skills_go_with_the_message_by_path() {
        let mut adapter = adapter();
        let skills = listed(
            &mut adapter,
            json!([
                {"cwd":"/repo","skills":[
                    {"name":"commits","description":"Write commits","path":"/u/commits/SKILL.md","scope":"user","enabled":true},
                    {"name":"lint","description":"Lint","path":"/u/lint/SKILL.md","scope":"user","enabled":true},
                    {"name":"lint","description":"Lint here","path":"/repo/lint/SKILL.md","scope":"repo","enabled":true},
                    {"name":"off","path":"/u/off/SKILL.md","enabled":false}
                ]},
                // The same skill reached from another directory is one skill.
                {"cwd":"/repo/sub","skills":[
                    {"name":"commits","description":"Write commits","path":"/u/commits/SKILL.md","scope":"user","enabled":true}
                ]}
            ]),
        );
        let shown = skills
            .iter()
            .map(|skill| (skill.invocation.as_str(), skill.description.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            shown,
            [
                ("$commits", "Write commits"),
                ("[$lint](/u/lint/SKILL.md)", "user · Lint"),
                ("[$lint](/repo/lint/SKILL.md)", "repo · Lint here"),
            ]
        );

        let text =
            "$commits then [$lint](/repo/lint/SKILL.md), $commits again, $lint, $off and $HOME";
        let input = adapter.input(text.into(), Vec::new()).unwrap();
        assert_eq!(
            input,
            [
                json!({"type":"text","text":text}),
                json!({"type":"skill","name":"commits","path":"/u/commits/SKILL.md"}),
                json!({"type":"skill","name":"lint","path":"/repo/lint/SKILL.md"}),
            ],
            "a bare shared name, a disabled skill and a variable name nothing"
        );
        assert_eq!(
            adapter.input("no skills here".into(), Vec::new()).unwrap(),
            [json!({"type":"text","text":"no skills here"})]
        );
    }

    #[test]
    fn a_symlinked_skill_is_one_named_skill() {
        use std::{fs, os::unix::fs::symlink};

        let directory = tempfile::tempdir().unwrap();
        let actual = directory.path().join("dotfiles/skills/review/SKILL.md");
        fs::create_dir_all(actual.parent().unwrap()).unwrap();
        fs::write(&actual, "# Review").unwrap();
        let link = directory.path().join("agents-skills");
        symlink(directory.path().join("dotfiles/skills"), &link).unwrap();
        let alias = link.join("review/SKILL.md");
        let mut adapter = adapter();
        let skills = listed(
            &mut adapter,
            json!([{"skills":[
                {"name":"review","description":"Review changes","path":actual},
                {"name":"review","description":"Review changes","path":alias}
            ]}]),
        );
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].invocation, "$review");
        assert_eq!(
            adapter.input("$review".into(), Vec::new()).unwrap(),
            [
                json!({"type":"text","text":"$review"}),
                json!({"type":"skill","name":"review","path":actual})
            ]
        );
    }

    #[test]
    fn a_link_to_a_symlink_target_uses_the_providers_listed_path() {
        use std::{fs, os::unix::fs::symlink};

        let directory = tempfile::tempdir().unwrap();
        let actual = directory.path().join("skills/review/SKILL.md");
        fs::create_dir_all(actual.parent().unwrap()).unwrap();
        fs::write(&actual, "# Review").unwrap();
        let alias = directory.path().join("alias.md");
        symlink(&actual, &alias).unwrap();
        let mut adapter = adapter();
        listed(
            &mut adapter,
            json!([{"skills":[{"name":"review","path":alias}]}]),
        );
        let message = format!("[$review]({})", actual.display());
        assert_eq!(
            adapter.input(message.clone(), Vec::new()).unwrap(),
            [
                json!({"type":"text","text":message}),
                json!({"type":"skill","name":"review","path":alias})
            ]
        );
    }

    #[test]
    fn distinct_same_scope_skills_show_different_sources() {
        let mut adapter = adapter();
        let skills = listed(
            &mut adapter,
            json!([{"skills":[
                {"name":"review","description":"Review changes","path":"/home/user/.agents/skills/review/SKILL.md","scope":"user"},
                {"name":"review","description":"Review changes","path":"/repo/.agents/skills/review/SKILL.md","scope":"user"}
            ]}]),
        );
        assert_eq!(skills.len(), 2);
        assert_eq!(
            skills[0].description,
            "user · user/.agents/skills · Review changes"
        );
        assert_eq!(
            skills[1].description,
            "user · repo/.agents/skills · Review changes"
        );
    }

    #[test]
    fn source_labels_distinguish_skill_directories_with_the_same_declared_name() {
        assert_eq!(
            skill_source(
                "/repo/skills/a/SKILL.md",
                ["/repo/skills/b/SKILL.md"].into_iter(),
            ),
            "a"
        );
        assert_eq!(
            skill_source(
                "/repo/skills/b/SKILL.md",
                ["/repo/skills/a/SKILL.md"].into_iter(),
            ),
            "b"
        );
    }

    #[test]
    fn mentions_are_read_the_way_codex_reads_them() {
        assert_eq!(
            skill_mentions(
                "[$a-b:c](/x/SKILL.md) and $d_e.$PATH $ [$f] (/y) cost$9 [$g](/z/SKILL.md"
            ),
            [
                Mention::Linked("/x/SKILL.md"),
                Mention::Named("d_e"),
                Mention::Linked("/y"),
                Mention::Named("9"),
                Mention::Named("g"),
            ]
        );
        assert!(skill_mentions("naïve $ é [$HOME](/h)").is_empty());
        assert_eq!(
            skill_mentions("[$x]( ) [$y]\u{a0}(/p)"),
            [Mention::Named("x"), Mention::Named("y"),]
        );
    }

    /// A skill part in an echoed message names what its text already names.
    #[test]
    fn skill_parts_of_a_user_message_add_nothing_to_show() {
        let echoed = json!({"id":"1","type":"userMessage","content":[
            {"type":"text","text":"$commits please"},
            {"type":"skill","name":"commits","path":"/u/commits/SKILL.md"},
            {"type":"mention","name":"x","path":"app://x"}
        ]});
        let items = parse_item(&echoed, true, "turn").unwrap();
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0].content,
            ItemContent::Text { text, .. } if text == "$commits please"));
    }

    #[test]
    fn first_prompt_waits_for_skill_paths_in_either_response_order() {
        const PROMPT: &str = "[$review](/skills/review/SKILL.md) this change";
        for skills_first in [false, true] {
            for catalog_supported in [false, true] {
                let mut adapter = adapter();
                adapter.thread = None;
                adapter.materialized = false;
                let initialize = adapter.initialize().unwrap();
                let setup = adapter.receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":"/tmp"}})).unwrap();
                let id = |method| {
                    setup
                        .writes
                        .iter()
                        .find(|frame| frame["method"] == method)
                        .unwrap()["id"]
                        .clone()
                };
                let attach = json!({"id":id("thread/start"),"result":{"thread":{"id":"fresh","canAcceptDirectInput":true,"status":{"type":"idle"}},"model":"model"}});
                let skills = if catalog_supported {
                    json!({"id":id("skills/list"),"result":{"data":[{"skills":[
                        {"name":"review","path":"/skills/review/SKILL.md"},
                        {"name":"review","path":"/plugins/review/SKILL.md"}
                    ]}]}})
                } else {
                    json!({"id":id("skills/list"),"error":{"code":-32601,"message":"Method not found"}})
                };
                let responses = if skills_first {
                    [skills, attach]
                } else {
                    [attach, skills]
                };
                let first = adapter.receive(responses[0].clone()).unwrap();
                assert!(!first.events.contains(&Event::Ready(true)));
                let prompt = || Command::Prompt {
                    text: PROMPT.into(),
                    attachments: Vec::new(),
                    mode: PromptMode::Send,
                };
                assert!(adapter.command(prompt()).is_err());
                let second = adapter.receive(responses[1].clone()).unwrap();
                assert!(second.events.contains(&Event::Ready(true)));
                let sent = adapter.command(prompt()).unwrap();
                let input = sent.writes[0]["params"]["input"].as_array().unwrap();
                assert_eq!(input[0], json!({"type":"text","text":PROMPT}));
                if catalog_supported {
                    assert_eq!(
                        input[1],
                        json!({"type":"skill","name":"review","path":"/skills/review/SKILL.md"})
                    );
                } else {
                    assert_eq!(input.len(), 1);
                }
                let refresh = adapter
                    .receive(json!({"method":"skills/changed","params":{}}))
                    .unwrap();
                assert!(
                    refresh
                        .writes
                        .iter()
                        .any(|frame| frame["method"] == "skills/list")
                );
                assert!(adapter.ready);
            }
        }
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
        // The first sender also waits for the skill catalog.
        assert!(!attached.events.contains(&Event::Ready(true)));
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
        let skills = setup
            .writes
            .iter()
            .find(|frame| frame["method"] == "skills/list")
            .unwrap();
        let catalog = adapter
            .receive(json!({"id":skills["id"],"result":{"data":[]}}))
            .unwrap();
        assert!(catalog.events.contains(&Event::Ready(true)));
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

    /// A command writes for a terminal, and Codex forwards what it wrote: a
    /// colour cut in two by the end of a delta must not reach the transcript
    /// as its second half, and the output reported whole at the end shows a
    /// redrawn line as it was last drawn.
    #[test]
    fn command_output_reaches_the_transcript_without_terminal_escapes() {
        let mut adapter = adapter();
        adapter.busy = true;
        let item = json!({"id":"0","type":"commandExecution","command":"cargo build","cwd":"/tmp","status":"inProgress","aggregatedOutput":"\u{1b}[1mStart\u{1b}[0m\n"});
        let mut page = Step::default();
        adapter.page(&json!({"data":[{"id":"turn-a","status":"inProgress","items":[item]}],"nextCursor":"older"}),&mut page, false).unwrap();
        let mut delta = |text: &str| {
            adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"turn-a","itemId":"0","delta":text}})).unwrap().events
        };
        // The introducer alone appends nothing, so there is nothing to send.
        assert!(delta("\u{1b}[1;3").is_empty());
        let events = delta("2m   Compiling\u{1b}[0m a\n 1/2\r");
        assert!(
            matches!(&events[0], Event::OutputDelta { offset: 6, delta, .. } if delta == "   Compiling a\n 1/2"),
            "{events:?}"
        );
        let events = delta(" 2/2\r\n");
        assert!(
            matches!(&events[0], Event::OutputDelta { offset: 25, delta, .. } if delta == "\n 2/2\n"),
            "{events:?}"
        );
        let completed = json!({"id":"0","type":"commandExecution","command":"cargo build","cwd":"/tmp","status":"completed","exitCode":0,"aggregatedOutput":"\u{1b}[1mStart\u{1b}[0m\n\u{1b}[1;32m   Compiling\u{1b}[0m a\n 1/2\r 2/2\r\n"});
        let done = adapter.receive(json!({"method":"item/completed","params":{"threadId":"thread-exact","turnId":"turn-a","item":completed}})).unwrap();
        assert!(done.events.iter().any(|event| matches!(event,
            Event::Item(Item { content: ItemContent::Command { output, .. }, .. })
            if output == "Start\n   Compiling a\n 2/2\n")));
        assert!(adapter.output_streams.is_empty());
    }

    #[test]
    fn resumed_step_and_final_history_use_same_turn_scoped_identity() {
        let mut adapter = adapter();
        adapter.busy = true;
        let item = json!({"id":"0","type":"commandExecution","command":"echo hello","cwd":"/tmp","status":"inProgress","aggregatedOutput":"hel"});
        let mut page = Step::default();
        adapter.page(&json!({"data":[{"id":"turn-a","status":"inProgress","items":[item]}],"nextCursor":"older"}),&mut page, false).unwrap();
        let delta = adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"turn-a","itemId":"0","delta":"lo"}})).unwrap();
        assert!(
            matches!(&delta.events[0],Event::OutputDelta { id, offset: 3, delta } if id == "turn-a/0" && delta == "lo")
        );
        let completed = json!({"id":"0","type":"agentMessage","text":"answer"});
        let one = parse_item(&completed, true, "turn-a").unwrap();
        let two = parse_item(&completed, true, "turn-b").unwrap();
        assert_ne!(one[0].id, two[0].id);
        let live = adapter.receive(json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact","turnId":"turn-a","itemId":"0","delta":"answer"}})).unwrap();
        assert!(matches!(&live.events[0],Event::TextDelta { id,.. } if id == &one[0].id));
    }

    #[test]
    fn long_active_turn_history_tracks_only_running_tools() {
        let mut adapter = adapter();
        adapter.busy = true;
        let mut items: Vec<_> = (0..MAX_ACTIVE_ITEMS + 32)
            .map(|index| json!({"id":index.to_string(),"type":"commandExecution","command":"echo done","cwd":"/repo","status":"completed","aggregatedOutput":"done"}))
            .collect();
        items.push(json!({"id":"running","type":"commandExecution","command":"echo live","cwd":"/repo","status":"inProgress","aggregatedOutput":"start"}));
        let turn = json!({"id":"long","status":"inProgress","items":items});
        let mut page = Step::default();
        adapter
            .page(
                &json!({"data":[turn.clone()],"nextCursor":null}),
                &mut page,
                false,
            )
            .unwrap();
        assert_eq!(adapter.active_items.len(), 1);
        assert!(
            page.events.iter().any(|event| matches!(event, Event::LatestHistory { items, .. } if items.len() == MAX_ACTIVE_ITEMS + 33))
        );
        let started = adapter
            .receive(
                json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":turn}}),
            )
            .unwrap();
        assert_eq!(adapter.active_items.len(), 1);
        assert_eq!(
            started
                .events
                .iter()
                .filter(|event| matches!(event, Event::Item(_)))
                .count(),
            MAX_ACTIVE_ITEMS + 33
        );
        let delta = adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"long","itemId":"running","delta":" tail"}})).unwrap();
        assert!(
            matches!(&delta.events[0], Event::OutputDelta { offset: 5, delta, .. } if delta == " tail")
        );
        let completed = json!({"id":"running","type":"commandExecution","command":"echo live","cwd":"/repo","status":"completed","aggregatedOutput":"start tail"});
        adapter.receive(json!({"method":"item/completed","params":{"threadId":"thread-exact","turnId":"long","item":completed}})).unwrap();
        assert!(adapter.active_items.is_empty());
    }

    #[test]
    fn orchestrating_turn_outlives_the_live_tool_bound() {
        // `sleep` and `subAgentActivity` state no status, so each reads as
        // running until its turn ends, whether it arrives in the turn's
        // history or as it starts.
        let auxiliary = |index: usize| {
            if index % 2 == 0 {
                json!({"id":format!("aux-{index}"),"type":"sleep","durationMs":1000})
            } else {
                json!({"id":format!("aux-{index}"),"type":"subAgentActivity","kind":"started","agentThreadId":"child","agentPath":"/root/review"})
            }
        };
        let mut adapter = adapter();
        adapter.busy = true;
        let mut items: Vec<_> = (0..MAX_ACTIVE_ITEMS * 2).map(auxiliary).collect();
        items.push(json!({"id":"running","type":"commandExecution","command":"echo live","cwd":"/repo","status":"inProgress","aggregatedOutput":"start"}));
        let turn = json!({"id":"long","status":"inProgress","items":items});
        let mut page = Step::default();
        adapter
            .page(
                &json!({"data":[turn.clone()],"nextCursor":null}),
                &mut page,
                false,
            )
            .unwrap();
        assert_eq!(adapter.active_items.len(), 1);
        adapter
            .receive(
                json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":turn}}),
            )
            .unwrap();
        for index in 0..MAX_ACTIVE_ITEMS * 2 {
            adapter.receive(json!({"method":"item/started","params":{"threadId":"thread-exact","turnId":"long","item":auxiliary(index)}})).unwrap();
        }
        assert_eq!(adapter.active_items.len(), 1);
        let delta = adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"long","itemId":"running","delta":" tail"}})).unwrap();
        assert!(
            matches!(&delta.events[0], Event::OutputDelta { offset: 5, delta, .. } if delta == " tail")
        );
    }

    #[test]
    fn running_tools_past_the_bound_go_unheld_without_ending_the_session() {
        let mut adapter = adapter();
        adapter.busy = true;
        let command = |index: usize, status: &str| json!({"id":index.to_string(),"type":"commandExecution","command":"sleep 1","cwd":"/repo","status":status,"aggregatedOutput":""});
        for index in 0..=MAX_ACTIVE_ITEMS {
            let started = adapter.receive(json!({"method":"item/started","params":{"threadId":"thread-exact","turnId":"turn","item":command(index, "inProgress")}})).unwrap();
            assert!(matches!(&started.events[0], Event::Item(_)));
        }
        assert_eq!(adapter.active_items.len(), MAX_ACTIVE_ITEMS);
        let unheld = adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"turn","itemId":MAX_ACTIVE_ITEMS.to_string(),"delta":"lost"}})).unwrap();
        assert!(unheld.events.is_empty());
        let completed = adapter.receive(json!({"method":"item/completed","params":{"threadId":"thread-exact","turnId":"turn","item":command(MAX_ACTIVE_ITEMS, "completed")}})).unwrap();
        assert!(matches!(
            &completed.events[0],
            Event::Item(Item {
                content: ItemContent::Command {
                    status: ToolStatus::Succeeded,
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn active_turn_history_releases_tools_that_completed_unseen() {
        let mut adapter = adapter();
        adapter.busy = true;
        let command = |status: &str| json!({"id":"cmd","type":"commandExecution","command":"echo hi","cwd":"/repo","status":status,"aggregatedOutput":"hi"});
        adapter.receive(json!({"method":"item/started","params":{"threadId":"thread-exact","turnId":"turn","item":command("inProgress")}})).unwrap();
        adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"turn","itemId":"cmd","delta":"\u{1b}["}})).unwrap();
        assert_eq!(adapter.active_items.len(), 1);
        assert_eq!(adapter.output_streams.len(), 1);
        let mut page = Step::default();
        adapter
            .page(
                &json!({"data":[{"id":"turn","status":"inProgress","items":[command("completed")]}],"nextCursor":null}),
                &mut page,
                false,
            )
            .unwrap();
        assert!(adapter.active_items.is_empty());
        assert!(adapter.output_streams.is_empty());
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
            panic!("missing bounded output window")
        };
        let ItemContent::Command { output: suffix, .. } = &streamed.content else {
            panic!("missing command")
        };
        assert!(suffix.len() <= MAX_ITEM_BYTES);
        assert!(suffix.contains("bytes omitted"));
        assert!(suffix.ends_with("🦀"));
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
        assert_eq!(label, "src/app.rs");
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

    /// A model listing that comes in pages is reported once it is whole: a
    /// model on a later page is one Codex offers, not one it lacks.
    #[test]
    fn a_paged_model_listing_is_reported_whole() {
        let mut adapter = adapter();
        let mut listing = Step::default();
        adapter
            .call(
                &mut listing,
                "model/list",
                json!({"limit":100}),
                Pending::Models,
            )
            .unwrap();
        let first = adapter
            .receive(json!({"id":listing.writes[0]["id"],"result":{"data":[{"model":"gpt-a"}],"nextCursor":"more"}}))
            .unwrap();
        assert!(
            !first
                .events
                .iter()
                .any(|event| matches!(event, Event::Models { .. })),
            "part of a listing is not reported"
        );
        assert_eq!(first.writes[0]["params"]["cursor"], "more");
        let last = adapter
            .receive(json!({"id":first.writes[0]["id"],"result":{"data":[{"model":"gpt-b"}]}}))
            .unwrap();
        let reported = last.events.iter().find_map(|event| match event {
            Event::Models { models, .. } => Some(
                models
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        });
        assert_eq!(reported, Some(vec!["gpt-a", "gpt-b"]));

        // A later page that fails leaves the pages that arrived.
        let mut second = self::adapter();
        let mut listing = Step::default();
        second
            .call(
                &mut listing,
                "model/list",
                json!({"limit":100}),
                Pending::Models,
            )
            .unwrap();
        let first = second
            .receive(json!({"id":listing.writes[0]["id"],"result":{"data":[{"model":"gpt-a"}],"nextCursor":"more"}}))
            .unwrap();
        let failed = second
            .receive(json!({"id":first.writes[0]["id"],"error":{"message":"busy"}}))
            .unwrap();
        assert!(failed.events.iter().any(|event| matches!(
            event,
            Event::Models { models, .. } if models.len() == 1 && models[0].id == "gpt-a"
        )));
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
    fn question_and_elicitation_kinds_preserve_their_exact_reply_contracts() {
        let mut adapter = adapter();
        let questions = adapter.receive(json!({"id":"question-kind","method":"item/tool/requestUserInput","params":{"threadId":"thread-exact","turnId":"turn","itemId":"question","questions":[{"id":"scope","header":"Scope","question":"Which?","isOther":false,"options":[{"label":"One"}]}]}})).unwrap();
        assert!(
            matches!(&questions.events[0], Event::Request(request) if request.kind == chat::RequestKind::Question && request.actions[0].id == "submit")
        );
        let submitted = adapter
            .command(Command::Answer {
                request_id: rpc_id(&json!("question-kind")).unwrap(),
                action: "submit".into(),
                answers: vec![Answer {
                    field: "scope".into(),
                    values: vec!["One".into()],
                }],
            })
            .unwrap();
        assert_eq!(
            submitted.writes[0]["result"],
            json!({"answers":{"scope":{"answers":["One"]}}})
        );
        for (id, action) in [("consent-decline", "decline"), ("consent-cancel", "cancel")] {
            let consent = adapter.receive(json!({"id":id,"method":"mcpServer/elicitation/request","params":{"threadId":"thread-exact","turnId":"turn","mode":"form","message":"Supply input","requestedSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}}})).unwrap();
            assert!(consent.events.iter().any(|event| matches!(event, Event::Request(request) if request.kind == chat::RequestKind::Consent && !request.fields.is_empty())));
            let response = adapter
                .command(Command::Answer {
                    request_id: rpc_id(&json!(id)).unwrap(),
                    action: action.into(),
                    answers: vec![],
                })
                .unwrap();
            assert_eq!(response.writes[0]["result"], json!({"action":action}));
        }
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
    #[test]
    fn resume_login_verifies_same_account_without_replaying_or_resetting_thread() {
        let mut adapter = adapter();
        let failure = "Your access token could not be refreshed. Please sign in again.";
        let expected = crate::account::Identity {
            email: Some("expected@example.test".into()),
            organization: None,
            plan: None,
        };
        let request = adapter
            .command(Command::ResumeAfterLogin {
                failure: failure.into(),
                expected: Some(expected.clone()),
            })
            .unwrap();
        assert_eq!(request.writes.len(), 1);
        assert_eq!(request.writes[0]["method"], "account/read");
        assert_eq!(request.writes[0]["params"], json!({"refreshToken":true}));
        assert!(request.events.is_empty());
        assert!(
            adapter
                .command(Command::ResumeAfterLogin {
                    failure: failure.into(),
                    expected: None
                })
                .is_err()
        );
        let result = adapter.receive(json!({"id":request.writes[0]["id"],"result":{"account":{"type":"chatgpt","email":"expected@example.test","planType":"pro"},"requiresOpenaiAuth":true}})).unwrap();
        assert_eq!(
            result.events,
            vec![Event::LoginRecovered {
                failure: failure.into()
            }]
        );
        assert!(result.writes.is_empty());
        assert_eq!(adapter.thread.as_deref(), Some("thread-exact"));
        for response in [
            json!({"account":null,"requiresOpenaiAuth":true}),
            json!({"account":{"type":"chatgpt","email":"different@example.test"}}),
            json!({"account":{"type":"apiKey"},"requiresOpenaiAuth":false}),
        ] {
            let request = adapter
                .command(Command::ResumeAfterLogin {
                    failure: failure.into(),
                    expected: Some(expected.clone()),
                })
                .unwrap();
            let result = adapter
                .receive(json!({"id":request.writes[0]["id"],"result":response}))
                .unwrap();
            assert!(
                matches!(&result.events[..], [Event::LoginVerificationFailed {failure: captured,..}] if captured == failure)
            );
            assert!(result.writes.is_empty());
        }
        let request = adapter
            .command(Command::ResumeAfterLogin {
                failure: failure.into(),
                expected: None,
            })
            .unwrap();
        let failed = adapter.receive(json!({"id":request.writes[0]["id"],"error":{"code":-32000,"message":"refresh token permanently invalid"}})).unwrap();
        assert!(
            matches!(&failed.events[..], [Event::LoginVerificationFailed {failure: captured,detail}] if captured == failure && detail.contains("verification failed"))
        );
        let request = adapter
            .command(Command::ResumeAfterLogin {
                failure: failure.into(),
                expected: None,
            })
            .unwrap();
        adapter.lifecycle_revision += 1;
        let late = adapter.receive(json!({"id":request.writes[0]["id"],"result":{"account":{"type":"chatgpt","email":"expected@example.test"}}})).unwrap();
        assert!(late.events.is_empty());
    }
    #[test]
    fn stale_login_verification_rpc_failure_emits_no_new_error() {
        let mut adapter = adapter();
        let request = adapter
            .command(Command::ResumeAfterLogin {
                failure: "Authentication required. Please sign in again.".into(),
                expected: None,
            })
            .unwrap();
        adapter.lifecycle_revision += 1;
        let late = adapter.receive(json!({"id":request.writes[0]["id"],"error":{"code":-32000,"message":"old refresh failed"}})).unwrap();
        assert!(late.events.is_empty());
    }
}
