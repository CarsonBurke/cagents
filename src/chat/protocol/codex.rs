//! Codex app-server JSON-RPC, verified against installed 0.159.3 schemas.
//! Wire contracts: `codex app-server generate-json-schema --experimental`,
//! https://github.com/openai/codex/tree/rust-v0.153.4/codex-rs/app-server-protocol
//! Pending requests are replayed to joining connections with the original ID;
//! the first answer wins (app-server/src/outgoing_message.rs). Never auto-answer.
use super::{Adapter, HeldStop, ProcessSpec, ServerIdentity, Step, TransportSpec};
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
pub(crate) mod rollout;

/// The Codex configuration key for a thread's reasoning effort. A launch
/// recipe names it as `--config model_reasoning_effort=<effort>`, the way the
/// Codex CLI takes it, and a new thread receives it as a `thread/start`
/// configuration override.
pub const REASONING_EFFORT_KEY: &str = crate::chat::fresh::CODEX_REASONING_EFFORT_KEY;

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
// Reserve for every possible byte count, leaving the rest for real output.
const OUTPUT_MARKER_BYTES: usize =
    "\n[...  bytes omitted ...]\n".len() + usize::MAX.ilog10() as usize + 1;
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
    if limit < OUTPUT_MARKER_BYTES + 4 {
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
    let budget = limit.saturating_sub(OUTPUT_MARKER_BYTES);
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
        && start + line + 1 < tail.len()
    {
        start += line + 1;
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
pub fn align_resume_identity(arguments: &mut [String], id: &str) -> Result<(), String> {
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

/// Where an account's shared Codex daemon listens, relative to its home.
const SHARED_CONTROL_SOCKET: &str = "app-server-control/app-server-control.sock";

/// The account home a Codex process launched from `spec` keeps its state in.
fn account_home(spec: &ProcessSpec) -> Result<PathBuf, &'static str> {
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
    Ok(if home.is_absolute() {
        home
    } else {
        spec.working_directory.join(home)
    })
}

/// The account's shared daemon endpoint, when `launch` would join it.
///
/// A bare `unix://` remote, without a socket named after it, selects the
/// shared daemon of the account home the launch runs under. Any named
/// endpoint is a request for that exact server, so none is reported.
pub(crate) fn shared_daemon_socket(launch: &Launch) -> Option<PathBuf> {
    let mut shared = false;
    let mut named = false;
    let mut args = launch.recipe.arguments.iter();
    while let Some(arg) = args.next() {
        let remote = match arg.as_str() {
            "--sock" => {
                args.next();
                named = true;
                continue;
            }
            "--remote" => args.next()?,
            _ if arg.starts_with("--remote=") => &arg["--remote=".len()..],
            _ if arg == "resume"
                || crate::agent::Provider::Codex.is_value_option(arg)
                || crate::agent::Provider::Codex.is_cwd_option(arg) =>
            {
                args.next();
                continue;
            }
            _ => continue,
        };
        shared = true;
        named = remote != "unix://";
    }
    let spec = ProcessSpec::from(&launch.recipe);
    (shared && !named)
        .then(|| account_home(&spec).ok())
        .flatten()
        .map(|home| home.join(SHARED_CONTROL_SOCKET))
}

/// The lock file whose holder Codex makes the one writer of `thread` in the
/// account home `launch` runs under. Codex holds it while the thread is loaded.
pub(crate) fn thread_writer_lock(launch: &Launch, thread: &str) -> Option<PathBuf> {
    let home = account_home(&ProcessSpec::from(&launch.recipe)).ok()?;
    Some(
        home.join("thread-writer-locks")
            .join(format!("{thread}.lock")),
    )
}

pub fn create(launch: &Launch) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    create_for_start(launch).map_err(|error| error.to_string())
}

pub(crate) fn create_for_start(
    launch: &Launch,
) -> Result<(ProcessSpec, Box<dyn Adapter>), super::SetupError> {
    create_inner(launch, None, None)
}

#[cfg(test)]
pub(crate) fn create_with_listener(
    launch: &Launch,
    socket: PathBuf,
) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    create_with_listener_for_start(launch, socket).map_err(|error| error.to_string())
}

pub(crate) fn create_with_listener_for_start(
    launch: &Launch,
    socket: PathBuf,
) -> Result<(ProcessSpec, Box<dyn Adapter>), super::SetupError> {
    let bytes = socket.as_os_str().as_encoded_bytes();
    // Match the actual platform Unix address before any provider can start.
    // All sockaddr_un fields are integers, so its all-zero value is valid.
    let capacity = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
        .sun_path
        .len();
    if socket.to_str().is_none()
        || bytes.is_empty()
        || bytes.contains(&0)
        || bytes.len() >= capacity
    {
        return Err(super::SetupError::Invalid(format!(
            "Owned Codex listener requires a nonempty, UTF-8, NUL-free Unix socket path shorter than {capacity} bytes; use a shorter UTF-8 temporary directory"
        )));
    }
    create_inner(launch, Some(socket), None)
}

/// Only authenticated holder recovery supplies this identity. The runtime
/// additionally verifies the exact retained app-server peer before initialize.
pub(crate) fn create_restored_for_start(
    recovery: &crate::chat::native_holder::Recovery,
) -> Result<(ProcessSpec, Box<dyn Adapter>), super::SetupError> {
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
) -> Result<(ProcessSpec, Box<dyn Adapter>), super::SetupError> {
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
                    remote_socket(args.next().ok_or("Missing Codex --remote address")?)
                        .map_err(super::SetupError::Invalid)?;
            }
            _ if arg.starts_with("--remote=") => {
                shared = true;
                explicit_socket =
                    remote_socket(&arg["--remote=".len()..]).map_err(super::SetupError::Invalid)?;
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
                let value = args.next().ok_or_else(|| {
                    super::SetupError::Invalid(format!("Missing value for Codex {arg}"))
                })?;
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
                let value = args.next().ok_or_else(|| {
                    super::SetupError::Invalid(format!("Missing value for Codex {arg}"))
                })?;
                // Only an override with a typed `thread/start` equivalent is
                // translated; any other key would silently change meaning.
                let effort = crate::chat::fresh::codex_config_effort(value)
                    .ok_or_else(|| {
                        super::SetupError::Invalid(format!(
                            "Codex launch option {arg} {value:?} cannot safely be translated to native app-server mode"
                        ))
                    })?;
                start["config"][REASONING_EFFORT_KEY] = json!(effort);
            }
            "--profile" | "-p" | "--config" | "-c" | "--add-dir" | "--remote-auth-token-env"
                if launch.session_id.is_some() =>
            {
                // The exact thread is already configured on the shared
                // server. Keep its live permissions/model, not TUI defaults.
                args.next().ok_or_else(|| {
                    super::SetupError::Invalid(format!("Missing value for Codex {arg}"))
                })?;
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
                return Err(super::SetupError::Invalid(format!(
                    "Codex launch option {arg:?} cannot safely be translated to native app-server mode"
                )));
            }
        }
    }
    let home = account_home(&spec)?;
    let expected_home = if let Some(identity) = &retained_identity {
        identity.canonical_home.clone()
    } else {
        std::fs::canonicalize(&home).map_err(|e| {
            super::SetupError::Unavailable(format!(
                "Cannot verify Codex account directory {}: {e}",
                home.display()
            ))
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
        let socket = explicit_socket.unwrap_or_else(|| home.join(SHARED_CONTROL_SOCKET));
        let socket = if socket.is_absolute() {
            socket
        } else {
            spec.working_directory.join(socket)
        };
        let metadata = std::fs::metadata(&socket).map_err(|error| super::SetupError::Unavailable(format!("Cannot join existing Codex daemon at {}: {error}. Start the Codex shared daemon first; no private fallback is launched.", socket.display())))?;
        if !metadata.file_type().is_socket() {
            return Err(super::SetupError::Unavailable(format!(
                "Codex endpoint {} is not a Unix socket",
                socket.display()
            )));
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
                .map_err(|e| {
                    super::SetupError::Unavailable(format!(
                        "Creating private Codex socket alias: {e}"
                    ))
                })?;
            let link = directory.join("s");
            if let Err(error) = std::os::unix::fs::symlink(&socket, &link) {
                let _ = std::fs::remove_dir(&directory);
                return Err(super::SetupError::Unavailable(format!(
                    "Creating exact Codex socket alias: {error}"
                )));
            }
            let verified = std::fs::metadata(&link)
                .is_ok_and(|m| m.dev() == metadata.dev() && m.ino() == metadata.ino());
            if !verified {
                let _ = std::fs::remove_file(&link);
                let _ = std::fs::remove_dir(&directory);
                return Err(super::SetupError::Unavailable(
                    "Codex socket changed during alias verification; reconnect explicitly".into(),
                ));
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
            cache_reading_total: 0,
            context_config: ContextConfig::default(),
            usage: None,
            server_identity: None,
            retained_identity,
            thread: launch.session_id.clone(),
            name_revision: 0,
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
            effective_reasoning: None,
            running_model: None,
            running_reasoning: None,
            model_turn: None,
            model_turn_settled: false,
            recent_model_turns: Default::default(),
            default_reasoning: Default::default(),
            pricing: Default::default(),
            turn_pricing: Default::default(),
            turn: None,
            held_stop: HeldStop::None,
            notification_turn: None,
            notification_revision: 0,
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
            child_names: BTreeMap::new(),
            branch_points: Vec::new(),
            is_child: false,
            child_failure: None,
            child_buffer: BTreeMap::new(),
            waiting_child_resumes: BTreeMap::new(),
            turn_diffs: BTreeMap::new(),
            repo_root: None,
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
    Attach {
        /// A verified connection may send a rename before resume answers.
        renamed: Option<String>,
    },
    ReadThread {
        name_revision: u64,
    },
    Models,
    Skills,
    History {
        earlier: bool,
        initial_notification_binding: Option<(u64, u64)>,
        adoption_binding: Option<(u64, u64)>,
    },
    Turn {
        revision: u64,
        notification_revision: u64,
        model: Option<String>,
        reasoning: Option<String>,
        tier: Option<String>,
        /// Completion can precede this exact request's acknowledgement.
        /// Only the reply's turn ID may promote one of these bounded frames.
        early: BTreeMap<String, EarlyNotification>,
        early_bytes: usize,
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
        reasoning: Option<String>,
    },
    QueueAdd,
    /// The effective configuration, for where the conversation compacts.
    Config,
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
/// The settings that move where Codex compacts a conversation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ContextConfig {
    /// `model_auto_compact_token_limit`.
    auto_compact: Option<u64>,
    /// `model_context_window`, which replaces the model's own window.
    window: Option<u64>,
}

impl ContextConfig {
    /// The share of a model's window Codex reports as usable, by default:
    /// the rest is headroom it keeps for the reply.
    const USABLE_PERCENT: u64 = 95;
    /// Codex compacts at nine tenths of the model's window, or at the
    /// configured limit where that comes first.
    const COMPACT_NUMERATOR: u64 = 9;
    const COMPACT_DENOMINATOR: u64 = 10;

    /// Where Codex compacts a conversation whose usage reading reports
    /// `usable` tokens of window: the reading gives the model's window less
    /// Codex's headroom, so the whole window is worked back from it, unless
    /// the configuration names the window itself.
    fn compact_limit(self, usable: u64) -> Option<u64> {
        let window = self.window.or_else(|| {
            (usable > 0).then(|| usable.saturating_mul(100).div_ceil(Self::USABLE_PERCENT))
        })?;
        let default = window.saturating_mul(Self::COMPACT_NUMERATOR) / Self::COMPACT_DENOMINATOR;
        Some(
            self.auto_compact
                .map_or(default, |limit| limit.min(default)),
        )
    }
}

#[derive(Default)]
struct Codex {
    /// The total input of the latest usage reading, which tells a new
    /// request's reading from a repeated one.
    cache_reading_total: u64,
    /// What the conversation's effective configuration says of its context,
    /// read once it connects.
    context_config: ContextConfig,
    /// The latest usage reading, given again with its compaction point
    /// should the configuration be read after it, as on a resume.
    usage: Option<Usage>,
    server_identity: Option<ServerIdentity>,
    retained_identity: Option<ServerIdentity>,
    thread: Option<String>,
    /// A rename notification can arrive while a metadata request is in flight.
    name_revision: u64,
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
    effective_reasoning: Option<String>,
    running_model: Option<String>,
    running_reasoning: Option<String>,
    /// Exact turn owning the displayed model, independent of the pricing
    /// estimator's provisional request matching.
    model_turn: Option<String>,
    model_turn_settled: bool,
    recent_model_turns: std::collections::VecDeque<String>,
    default_reasoning: BTreeMap<String, String>,
    pricing: chat::pricing::CostTracker,
    turn_pricing: std::collections::VecDeque<(String, Option<String>, Option<String>)>,
    turn: Option<String>,
    /// A stop asked for while this thread's `turn/start` was unanswered,
    /// sent as `turn/interrupt` once the turn's ID is known.
    held_stop: HeldStop,
    /// Exact live notice authority. Scalar status and paged history must not
    /// replace it; they can arrive after a newer turn's start.
    notification_turn: Option<String>,
    notification_revision: u64,
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
    /// Where the nickname shown for each child was learned, by child ID. It
    /// outlives a child's parser, which a crowded conversation can evict and
    /// readmit.
    child_names: BTreeMap<String, ChildName>,
    branch_points: Vec<chat::BranchPoint>,
    is_child: bool,
    /// The turn and message of the last final error a child reported, which
    /// Codex repeats in that turn's failed completion: one failure, one item.
    child_failure: Option<(String, String)>,
    child_buffer: BTreeMap<String, Vec<Value>>,
    /// Children whose thread read waits for their parent child's, by parent.
    waiting_child_resumes: BTreeMap<String, Vec<String>>,
    /// Each running turn's latest cumulative diff and the files its own
    /// file-change items edited. The part of the diff no item shows, such as
    /// a file a shell command rewrote, is shown as the turn's own change.
    turn_diffs: BTreeMap<String, TurnDiff>,
    /// The repository the session's directory is in, which a turn diff's
    /// paths are relative to, once looked up.
    repo_root: Option<Option<PathBuf>>,
}

/// Where the nickname shown for a child was learned, from oldest to newest.
/// Codex reports a child's nickname live, in its thread listing and in the
/// records of earlier calls, and the replies to listing and history requests
/// can arrive after a newer live notification.
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum ChildName {
    /// From a call a history page records, as the child was then.
    Recorded,
    /// From the thread listing, as the child was when it was requested.
    Listed,
    /// From a live notification or the child's own thread read.
    Current,
}

/// The most children whose nickname origin is remembered; beyond it, a new
/// child's historical name is published as if nothing were known of it.
const MAX_CHILD_NAMES: usize = 1024;

#[derive(Default)]
struct TurnDiff {
    diff: String,
    /// Each completed file-change item of the turn, by its ID, with the
    /// lines it changed in each file, by absolute path, as `+line` or
    /// `-line`. An item reported again replaces itself.
    edited: BTreeMap<String, Vec<(String, Vec<String>)>>,
    shown: bool,
}

#[derive(Clone, Default)]
struct EarlyNotification {
    completed: Option<Value>,
    /// Later activity cannot be undone by an old RPC reply after scalar idle.
    superseded: bool,
    preview: crate::chat::notification::Preview,
    preview_transferred: bool,
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
    /// The diff state of `turn`, kept for the few turns still running.
    fn turn_diff(&mut self, turn: &str) -> &mut TurnDiff {
        if !self.turn_diffs.contains_key(turn) && self.turn_diffs.len() >= 16 {
            self.turn_diffs.pop_first();
        }
        self.turn_diffs.entry(turn.into()).or_default()
    }

    /// Note the lines a completed file-change item changed, so the turn's
    /// diff shows only what no item did, and show the diff again.
    fn note_file_change(&mut self, turn: &str, value: &Value, step: &mut Step) {
        if value["type"] != "fileChange" || value["status"] != "completed" {
            return;
        }
        let Some(item) = value["id"].as_str() else {
            return;
        };
        let mut noted = Vec::new();
        for change in value["changes"].as_array().into_iter().flatten() {
            let lines = changed_lines(
                change["diff"].as_str().unwrap_or_default(),
                change["kind"]["type"].as_str(),
            );
            for path in [&change["path"], &change["kind"]["move_path"]] {
                if let Some(path) = path.as_str() {
                    noted.push((
                        self.cwd.join(path).to_string_lossy().into_owned(),
                        lines.clone(),
                    ));
                }
            }
        }
        self.turn_diff(turn).edited.insert(item.into(), noted);
        self.show_turn_diff(turn, step);
    }

    /// Show the changes the turn's diff holds that none of its file-change
    /// items made, such as a file a shell command rewrote, as one change row
    /// for the turn, or retract that row once items account for all of them.
    fn show_turn_diff(&mut self, turn: &str, step: &mut Step) {
        if !self.turn_diffs.contains_key(turn) {
            return;
        }
        let root = self
            .repo_root
            .get_or_insert_with(|| {
                self.cwd
                    .ancestors()
                    .find(|dir| crate::git_marker::present(&dir.join(".git")).unwrap_or(false))
                    .map(std::path::Path::to_path_buf)
            })
            .clone()
            .unwrap_or_else(|| self.cwd.clone());
        let state = self.turn_diffs.get_mut(turn).expect("checked above");
        let absolute = |path: &str| root.join(path).to_string_lossy().into_owned();
        let changes = turn_diff_changes(&state.diff)
            .into_iter()
            .map(|mut change| {
                change.path = absolute(&change.path);
                if let ChangeKind::Renamed { from } = &mut change.kind {
                    *from = absolute(from);
                }
                change
            })
            .filter(|change| {
                let mut paths = vec![change.path.as_str()];
                if let ChangeKind::Renamed { from } = &change.kind {
                    paths.push(from);
                }
                let edits = state
                    .edited
                    .values()
                    .flatten()
                    .filter(|(path, _)| paths.contains(&path.as_str()))
                    .flat_map(|(_, lines)| lines)
                    .collect::<Vec<_>>();
                // Explained when the items edited the file and made every
                // line of the turn's change to it, as often as it appears.
                // No item changes a file's mode, so a mode change is never.
                edits.is_empty() || change.diff.starts_with("old mode ") || {
                    let mut made = BTreeMap::<&str, usize>::new();
                    for line in edits {
                        *made.entry(line).or_default() += 1;
                    }
                    !changed_lines(&change.diff, None).iter().all(|line| {
                        made.get_mut(line.as_str())
                            .filter(|count| **count > 0)
                            .map(|count| *count -= 1)
                            .is_some()
                    })
                }
            })
            .collect::<Vec<_>>();
        let id = format!("{turn}/diff");
        if changes.is_empty() {
            if std::mem::take(&mut state.shown) {
                step.events.push(Event::ItemsRemoved { ids: vec![id] });
            }
            return;
        }
        state.shown = true;
        step.events.push(Event::Item(Item {
            id,
            role: Role::Assistant,
            content: ItemContent::Changes {
                changes,
                status: ToolStatus::Succeeded,
            },
            started_at: None,
            at: None,
        }));
    }

    /// Discover or refresh a child from live metadata, whose nickname is
    /// the child's current one.
    fn discover_child(
        &mut self,
        id: &str,
        metadata: &Value,
        step: &mut Step,
    ) -> Result<(), String> {
        self.discover_child_named(id, metadata, ChildName::Current, false, step)
            .map(drop)
    }

    /// Discover or refresh a child, publishing the nickname in `metadata`
    /// only where its `origin` may replace what is already known: a newer
    /// source replaces an older one, and a recorded call replaces another
    /// only when `advance` says the same history page set it. Returns
    /// whether a nickname was published.
    fn discover_child_named(
        &mut self,
        id: &str,
        metadata: &Value,
        origin: ChildName,
        advance: bool,
        step: &mut Step,
    ) -> Result<bool, String> {
        if self.is_child || id.is_empty() || id.len() > 256 || self.thread.as_deref() == Some(id) {
            return Ok(false);
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
            let waiting = self
                .waiting_child_resumes
                .values()
                .map(Vec::len)
                .sum::<usize>();
            if self.children.len() >= 32 || self.pending.len() + waiting >= MAX_PENDING {
                return Ok(false);
            }
            let mut child = Codex::default();
            child.thread = Some(id.into());
            child.cwd = self.cwd.clone();
            child.expected_home = self.expected_home.clone();
            child.is_child = true;
            self.children.insert(id.into(), Box::new(child));
            self.child_buffer.insert(id.into(), Vec::new());
            // Codex resumes a nested child only through its loaded parent and
            // refuses it while that parent's own read is still outstanding,
            // as it is when one listing discovers both.
            match parent_thread(metadata).filter(|parent| self.child_resume_pending(parent)) {
                Some(parent) => self
                    .waiting_child_resumes
                    .entry(parent.into())
                    .or_default()
                    .push(id.into()),
                None => self.resume_child(id, step)?,
            }
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
        let nickname = agent_nickname(metadata).filter(|_| {
            self.child_names.get(id).is_none_or(|known| {
                origin > *known || origin == *known && (advance || origin != ChildName::Recorded)
            })
        });
        if nickname.is_some() {
            self.note_child_name(id, origin);
        }
        step.events.push(Event::Subagent {
            model,
            waiting: None,
            detail: None,
            id: id.into(),
            // A role says what an agent can do, not who it is. Sparse,
            // role-only or superseded metadata publishes no name, so the
            // runtime keeps the nickname it already has; an unnamed child is
            // left to the tab's own fallback.
            kind: nickname.unwrap_or_default().into(),
            description: metadata["message"]
                .as_str()
                .or_else(|| metadata["preview"].as_str())
                .unwrap_or_default()
                .into(),
            status,
            // A child is a thread of its own, not work inside the parent's turn.
            background: Some(true),
        });
        Ok(nickname.is_some())
    }
    fn resume_child(&mut self, id: &str, step: &mut Step) -> Result<(), String> {
        self.call(
            step,
            "thread/resume",
            json!({"threadId":id,"excludeTurns":true,"initialTurnsPage":{"limit":4,"sortDirection":"desc","itemsView":"full"}}),
            Pending::ChildResume(id.into()),
        )
    }
    fn child_resume_pending(&self, id: &str) -> bool {
        self.pending
            .values()
            .any(|pending| matches!(pending, Pending::ChildResume(child) if child == id))
            || self
                .waiting_child_resumes
                .values()
                .any(|waiting| waiting.iter().any(|child| child == id))
    }
    /// Read the children that waited for `parent`'s read, now answered. A
    /// failed parent read is still the earliest a child's read can succeed,
    /// and its own failure is reported like any other.
    fn resume_waiting_children(&mut self, parent: &str, step: &mut Step) -> Result<(), String> {
        for child in self
            .waiting_child_resumes
            .remove(parent)
            .unwrap_or_default()
        {
            if self.pending.len() >= MAX_PENDING {
                self.child_read_failed(&child, "Too many outstanding Codex requests", step)?;
            } else {
                self.resume_child(&child, step)?;
            }
        }
        Ok(())
    }
    /// Hydration failed, not execution. Keep the live parser and replay the
    /// events observed while the read was pending. A child that showed no
    /// sign of life meanwhile isn't running here: whatever said it was, an
    /// old call's record or no status at all, predates this server, which
    /// has nothing loaded that a stop could reach.
    fn child_read_failed(
        &mut self,
        id: &str,
        message: &str,
        step: &mut Step,
    ) -> Result<(), String> {
        step.events.extend(chat::child_events(
            id,
            vec![session_notice(format!(
                "Child history unavailable: {message}. Live activity is still shown."
            ))],
        ));
        if let Some(child) = self.children.get_mut(id) {
            child.ready = true;
        }
        let buffered = self.child_buffer.remove(id).unwrap_or_default();
        let quiet = buffered.is_empty();
        for frame in buffered {
            if let Some(child) = self.children.get_mut(id) {
                let content = child.receive(frame)?;
                self.child_step(id, content, step)?;
            }
        }
        if quiet
            && let Some(child) = self.children.get_mut(id)
            && !child.busy
        {
            child.settle_idle_child(id, step);
        }
        self.resume_waiting_children(id, step)
    }
    /// Publish a child with no turn as settled: the outcome it already has,
    /// or stopped where it was only ever thought to be running.
    fn settle_idle_child(&mut self, id: &str, step: &mut Step) {
        let status = self
            .subagent_status
            .filter(|status| *status != ToolStatus::Running)
            .unwrap_or(ToolStatus::Stopped);
        self.subagent_status = Some(status);
        self.subagent_waiting = false;
        step.events.push(Event::Subagent {
            id: id.into(),
            kind: String::new(),
            description: String::new(),
            status: Some(status),
            background: None,
            model: None,
            waiting: Some(false),
            detail: None,
        });
    }
    /// Publish the nickname a child's own thread read reports, which can name
    /// a child discovered without one. A name already current, such as the
    /// one in the live notification that requested the read, stands: Codex
    /// names a child once, when it spawns it, so a read never corrects one.
    fn read_child_name(&mut self, id: &str, thread: &Value, step: &mut Step) {
        let Some(nickname) = agent_nickname(thread) else {
            return;
        };
        if self.child_names.get(id) == Some(&ChildName::Current) {
            return;
        }
        self.note_child_name(id, ChildName::Current);
        step.events.push(Event::Subagent {
            id: id.into(),
            kind: nickname.into(),
            description: String::new(),
            status: None,
            background: None,
            model: None,
            waiting: None,
            detail: None,
        });
    }
    fn note_child_name(&mut self, id: &str, origin: ChildName) {
        if self.child_names.len() < MAX_CHILD_NAMES || self.child_names.contains_key(id) {
            self.child_names.insert(id.into(), origin);
        }
    }
    // Child parsers share a server connection, so their local RPC IDs must
    // never escape into the parent's ID space. Keep the original correlation
    // inside the child and allocate every wire request through the parent.
    fn child_step(&mut self, id: &str, mut content: Step, step: &mut Step) -> Result<(), String> {
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
        // Client/RPC errors belong to the child too, but do not prove a
        // provider-marked failure. Genuine provider errors already carry
        // their provenance from the notification decoder. Preserve other
        // diagnostics as plain notices without changing lifecycle state.
        for event in &mut content.events {
            if let Event::Error { message, .. } = event {
                *event = session_notice(std::mem::take(message));
            }
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
    fn finish_execution(
        &mut self,
        turn: &Value,
        step: &mut Step,
        publish_content: bool,
    ) -> Result<(), String> {
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
        // An ACK can recover a completion whose content arrived before exact
        // execution binding. Ordinary completed frames already publish it.
        if publish_content {
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
            let mut synthetic = std::collections::HashSet::new();
            let mut references = std::collections::HashMap::new();
            let mut unverified = std::collections::HashSet::new();
            for (index, pair) in inputs.windows(2).enumerate() {
                if pair[0]["type"] == "text"
                    && pair[1]["type"] == "localImage"
                    && let (Some(marker), Some(path)) =
                        (pair[0]["text"].as_str(), pair[1]["path"].as_str())
                    && super::image_references::claims(marker, path)
                {
                    if let Some(reference) = super::image_references::restore(marker, path) {
                        synthetic.insert(index);
                        references.insert(index + 1, reference);
                    } else {
                        // A canonical owned-image claim is not a legacy local
                        // file. On another device the same path can mean
                        // different pixels; never offer it for edit/replay.
                        unverified.insert(index + 1);
                    }
                }
            }
            for (index, input) in inputs.iter().enumerate() {
                if synthetic.contains(&index) {
                    continue;
                }
                parts.push(match input["type"].as_str() {
                    Some("text") => string(input, "text")?.to_owned(),
                    Some("localImage") => {
                        attachments.push(chat::Attachment {
                            path: if unverified.contains(&index) {
                                std::path::PathBuf::new()
                            } else {
                                string(input, "path")?.into()
                            },
                            reference: references.remove(&index),
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
                .enumerate()
                .filter(|(index, input)| input["type"] == "text" && !synthetic.contains(index))
                .map(|(_, input)| input)
                .collect::<Vec<_>>();
            let named = texts
                .first()
                .and_then(|input| input["text"].as_str())
                .map(|text| self.named_skills(text))
                .unwrap_or_default();
            message.editable &= unverified.is_empty()
                && texts.len() <= 1
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
    fn active_model_event(&self, step: &mut Step) {
        step.events
            .push(Event::ActiveModel(self.running_model.as_ref().map(|id| {
                chat::ActiveModel {
                    id: id.clone(),
                    label: self
                        .models
                        .iter()
                        .find(|model| &model.id == id)
                        .map_or_else(|| id.clone(), |model| model.label.clone()),
                    reasoning: self.running_reasoning.clone(),
                    default_reasoning: self.default_reasoning.get(id).cloned(),
                    aliases: Vec::new(),
                }
            })));
    }
    fn model_events(&self, step: &mut Step) {
        step.events.push(Event::Models {
            models: self.models.clone(),
            selected: self.selected.clone(),
            reasoning: self.reasoning.clone(),
        });
        self.active_model_event(step);
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
    fn remember_model_turn(&mut self, id: &str) {
        if self.recent_model_turns.iter().any(|turn| turn == id) {
            return;
        }
        if self.recent_model_turns.len() >= 32 {
            self.recent_model_turns.pop_front();
        }
        self.recent_model_turns.push_back(id.to_owned());
    }
    fn bind_model_turn(&mut self, id: &str) {
        self.remember_model_turn(id);
        if self.model_turn.as_deref() != Some(id) {
            self.model_turn_settled = false;
        }
        self.model_turn = Some(id.to_owned());
    }
    fn history(&mut self, step: &mut Step, cursor: Option<String>) -> Result<(), String> {
        if !self.materialized {
            return Ok(());
        }
        if self.history_loading {
            return Err("Codex history is already loading".into());
        }
        let earlier = cursor.is_some();
        let initial_notification_binding = (!self.history_initialized
            && self.notification_turn.is_none())
        .then_some((self.lifecycle_revision, self.notification_revision));
        let adoption_binding =
            (!earlier).then_some((self.lifecycle_revision, self.notification_revision));
        self.call(step, "thread/turns/list", json!({"threadId":self.thread,"cursor":cursor,"limit":20,"sortDirection":"desc","itemsView":"full"}), Pending::History { earlier, initial_notification_binding, adoption_binding })?;
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
            Pending::ReadThread {
                name_revision: self.name_revision,
            },
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
    /// A native snapshot and its clipping/terminal decoder state have the same
    /// owner. Replacement starts a new decoder and installs the window produced
    /// by parsing THAT snapshot. History only seeds an absent active item.
    fn track_snapshot_item(
        &mut self,
        value: &Value,
        item: &Item,
        window: Option<OutputWindow>,
        replace: bool,
    ) {
        if self.tracks(value, item) {
            if replace || !self.active_items.contains_key(&item.id) {
                self.output_streams.remove(&item.id);
                self.output_windows.remove(&item.id);
                if value["type"] == "commandExecution" {
                    let raw = value["aggregatedOutput"].as_str().unwrap_or_default();
                    self.output_streams.insert(
                        item.id.clone(),
                        super::terminal_output::Stream::from_snapshot(raw),
                    );
                }
                if let Some(window) = window {
                    self.output_windows.insert(item.id.clone(), window);
                }
                self.active_items.insert(item.id.clone(), item.clone());
            }
        } else {
            self.active_items.remove(&item.id);
            self.output_streams.remove(&item.id);
            self.output_windows.remove(&item.id);
        }
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

    fn page(
        &mut self,
        page: &Value,
        step: &mut Step,
        earlier: bool,
    ) -> Result<Option<String>, String> {
        let mut items = Vec::new();
        let turns = array(page, "data")?;
        // Children whose nickname this page set. The page is read oldest
        // first, so a later call on it may advance that name. A name known
        // before the page stands: it came from a listing, a live
        // notification or a newer page, or, when the latest page is read
        // again, from that page's earlier reading.
        let mut named = std::collections::BTreeSet::new();
        for turn in turns.iter().rev() {
            let turn_id = string(turn, "id")?;
            let active = turn["status"] == "inProgress";
            if turn["itemsView"]
                .as_str()
                .is_some_and(|view| view != "full")
            {
                return Err("Codex did not return full turn items as requested; history cannot be hydrated safely".into());
            }
            for value in array(turn, "items")? {
                if !active
                    && value["type"] == "userMessage"
                    && let Some(label) = value["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find_map(|input| input["text"].as_str())
                        .and_then(crate::account::clean_label)
                    && !self.branch_points.iter().any(|point| point.id == turn_id)
                    && self.branch_points.len() < 128
                {
                    self.branch_points.push(chat::BranchPoint {
                        id: turn_id.into(),
                        label,
                    });
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
                        if self.discover_child_named(
                            id,
                            &metadata,
                            ChildName::Recorded,
                            named.contains(id),
                            step,
                        )? {
                            named.insert(id.to_owned());
                        }
                    }
                }
                if active {
                    // A turn still running when this conversation attached:
                    // its later diffs must not show again what these did.
                    self.note_file_change(turn_id, value, step);
                }
                let mut windows = BTreeMap::new();
                let parsed = parse_item_with_windows(value, !active, turn_id, |id, window| {
                    windows.insert(id.to_owned(), window);
                })?;
                for item in parsed {
                    if active && self.busy {
                        // Completed history releases an unseen completion;
                        // live history cannot replace a more recent stream.
                        self.track_snapshot_item(value, &item, windows.remove(&item.id), false);
                    }
                    items.push(item);
                }
            }
            if turn["status"] == "interrupted" {
                items.push(stopped_marker(turn_id));
            }
        }
        if !earlier {
            self.child_history_outcome(turns.first(), step);
        }
        // Refreshing the live end must not rewind the older-history traversal
        // through pages already cached. Commit only after validating the page;
        // a failed decode must preserve the cursor the reader can retry.
        if !earlier
            && !self.history_initialized
            && !self.is_child
            && page["nextCursor"].is_string()
            && let Some(id) = &self.thread
            && let Some(title) = rollout::opening_title(&self.expected_home, id)
        {
            step.events.push(Event::OpeningTitle(title));
        }
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
        turns
            .first()
            .filter(|turn| turn["status"] == "inProgress")
            .map(|turn| string(turn, "id").map(str::to_owned))
            .transpose()
    }
    fn input(&self, text: String, attachments: Vec<Attachment>) -> Result<Vec<Value>, String> {
        let mut input = Vec::with_capacity(1 + attachments.len());
        // Each skill the text names goes with it as an input of its own, as
        // Codex's own composer sends one: the text names a skill only by its
        // name, which loads nothing when two skills share it.
        let attachments = super::image_references::ordered(&text, &attachments)?;
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
            let attachment = crate::chat::attachment_assets::resolve(attachment)?;
            if !self.capabilities().attachments {
                return Err("The selected Codex model does not advertise image input".into());
            }
            let path = if attachment.path.is_absolute() {
                attachment.path.clone()
            } else {
                self.cwd.join(&attachment.path)
            };
            if !crate::chat::is_image_path(&path) {
                return Err("Codex native attachments support local images (PNG/JPEG/GIF/WebP), not arbitrary files".into());
            }
            if !path.is_file() {
                return Err(format!("Attachment is not a file: {}", path.display()));
            }
            // Codex wraps the pixels in its own tag naming the path.
            if let Some(reference) = &attachment.reference {
                input.push(
                    json!({"type":"text","text":super::image_references::caption(reference)?}),
                );
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
                let details: serde_json::Map<String, Value> =
                    ["kind", "additionalPermissions", "networkApprovalContext"]
                        .into_iter()
                        .filter(|key| !params[*key].is_null())
                        .map(|key| (key.to_owned(), params[key].clone()))
                        .collect();
                if !details.is_empty() {
                    description.push('\n');
                    description.push_str(&crate::chat::readable::value(&Value::Object(details)));
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
                    "\nRequested permissions:\n{}",
                    crate::chat::readable::value(&params["permissions"])
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
                // A decision with terms, such as a rule it would add, says
                // what they are; its own name is the label.
                description: match value.as_object() {
                    Some(decision) if decision.len() == 1 => decision
                        .values()
                        .next()
                        .map(crate::chat::readable::value)
                        .unwrap_or_default(),
                    Some(_) => crate::chat::readable::value(value),
                    None => String::new(),
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
    #[cfg(feature = "host")]
    fn needs_private_temp(&self) -> bool {
        true
    }

    fn answer_declines(&self, request_id: &str, action: &str) -> bool {
        let Some(request) = self.requests.get(request_id) else {
            return false;
        };
        let Some(choice) = request.actions.get(action) else {
            return false;
        };
        match request.kind {
            RequestKind::Decision | RequestKind::Elicitation => {
                matches!(choice.as_str(), Some("decline" | "cancel"))
            }
            RequestKind::Permissions => action == "deny",
            RequestKind::Questions => false,
        }
    }

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
                let input = self.input(text, attachments.clone())?;
                match mode {
                    PromptMode::Send if self.busy || self.pending.values().any(|p| matches!(p, Pending::Turn { .. })) => return Err("A Codex turn is active; explicitly steer or queue instead".into()),
                    PromptMode::Steer if self.turn.is_none() => return Err("There is no known active Codex turn to steer".into()),
                    _ => {}
                }
                for attachment in &attachments { crate::chat::attachment_assets::retain(attachment)?; }
                match mode {
                    PromptMode::Send => {
                        if self.busy || self.pending.values().any(|p| matches!(p, Pending::Turn { .. })) { return Err("A Codex turn is active; explicitly steer or queue instead".into()); }
                        self.held_stop = HeldStop::None;
                        let mut params = json!({"threadId":self.thread,"input":input});
                        if self.selection_dirty { params["model"] = json!(self.selected); params["effort"] = json!(self.reasoning); }
                        self.notification_revision = self.notification_revision.wrapping_add(1);
                        self.call(&mut step, "turn/start", params, Pending::Turn { revision: self.lifecycle_revision, notification_revision: self.notification_revision, model: self.selected.clone(), reasoning: self.reasoning.clone(), tier: self.service_tier.clone(), early: BTreeMap::new(), early_bytes: 0 })?;
                        self.notification_turn = None;
                        step.notification_preview = Some(Default::default());
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
            // Codex names a turn only once it has started, so a stop given
            // while its start is unanswered waits for that name.
            Command::Interrupt => match self.turn.clone() {
                Some(turn) => self.call(&mut step, "turn/interrupt", json!({"threadId":self.thread,"turnId":turn}), Pending::Other)?,
                None if self.starting_turn() => self.held_stop = HeldStop::Waiting,
                None => return Err("There is no known active Codex turn to interrupt".into()),
            },
            // The client's form of the model's own `interrupt_agent`: a child
            // is a thread of its own, so interrupting its turn reaches neither
            // the parent nor its siblings. Unlike input, this is not refused
            // for a child the server keeps from direct use.
            Command::StopSubagent { id } => {
                let child = self.children.get_mut(&id).ok_or("This Codex subagent is no longer followed")?;
                match child.turn.clone() {
                    Some(turn) => self.call(&mut step, "turn/interrupt", json!({"threadId":id,"turnId":turn}), Pending::StopChild)?,
                    // Working, but Codex hasn't named the turn a stop must target.
                    None if child.busy || child.starting_turn() => return Err("This Codex subagent's current turn isn't known yet; stop it again in a moment".into()),
                    // Nothing runs: settle whatever still showed it working.
                    None => child.settle_idle_child(&id, &mut step),
                }
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
                    reasoning: self.reasoning.clone(),
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
            Command::ReadTextCopy { .. } =>
                return Err("Text copies are served by the transcript cache".into()),
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
    fn held_stop(&self) -> HeldStop {
        self.held_stop
    }
    fn receive(&mut self, frame: Value) -> Result<Step, String> {
        let mut step = self.receive_frame(frame)?;
        if self.held_stop == HeldStop::Waiting {
            if let Some(turn) = self.turn.clone() {
                self.held_stop = HeldStop::Sent;
                self.call(
                    &mut step,
                    "turn/interrupt",
                    json!({"threadId":self.thread,"turnId":turn}),
                    Pending::Other,
                )?;
            } else if !self.starting_turn() {
                // The start failed: there is nothing left to stop.
                self.held_stop = HeldStop::Dropped;
            }
        }
        Ok(step)
    }
}

impl Codex {
    /// Whether a `turn/start` this thread sent is still unanswered.
    fn starting_turn(&self) -> bool {
        self.pending
            .values()
            .any(|pending| matches!(pending, Pending::Turn { .. }))
    }

    fn receive_frame(&mut self, frame: Value) -> Result<Step, String> {
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
                    self.child_read_failed(id, message, &mut step)?;
                    return Ok(step);
                }
                if matches!(pending, Pending::Prediction) {
                    if error["code"].as_i64() == Some(-32601) {
                        self.predictions_unsupported = true;
                    }
                    // Predictions are optional. Failure cannot affect chat.
                    return Ok(step);
                }
                // A server without the read compacts where its model's
                // default says, which the usage reading still gives.
                if matches!(pending, Pending::ChildList | Pending::Config) {
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
                if let Pending::Turn {
                    notification_revision,
                    ..
                } = &pending
                    && *notification_revision != self.notification_revision
                {
                    // A rejected old start cannot replace a newer turn's
                    // error. Its exact managed execution is handled below.
                    step.suppress_current_notification = true;
                }
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
                let fatal = matches!(pending, Pending::Initialize | Pending::Attach { .. });
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
                        .unwrap_or_else(|| error.to_string())
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
                    let pending = Pending::Attach { renamed: None };
                    if self.thread.is_some() {
                        self.call(&mut step, "thread/resume", json!({"threadId":self.thread,"excludeTurns":true,"initialTurnsPage":{"limit":20,"sortDirection":"desc","itemsView":"full"}}), pending)?;
                    } else {
                        self.call(&mut step, "thread/start", self.start.clone(), pending)?;
                    }
                    self.call(
                        &mut step,
                        "model/list",
                        json!({"limit":100}),
                        Pending::Models,
                    )?;
                    // Configuration layers - the home's, the project's,
                    // profiles and overrides - are Codex's to resolve.
                    self.call(
                        &mut step,
                        "config/read",
                        json!({"includeLayers":false,"cwd":self.cwd}),
                        Pending::Config,
                    )?;
                    self.skills(&mut step)?;
                }
                Pending::Attach { renamed } => {
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
                    self.effective_reasoning = self.reasoning.clone();
                    self.model_turn = None;
                    self.running_model = self.effective_model.clone();
                    self.running_reasoning = self.effective_reasoning.clone();
                    self.busy = thread["status"]["type"] == "active";
                    step.events.push(Event::PermissionMode(
                        permission_mode(&result["approvalPolicy"], &result["sandbox"])
                            .map(str::to_owned),
                    ));
                    step.events
                        .push(Event::Waiting(waiting_on_user(&thread["status"])));
                    if renamed.is_some() {
                        self.name_revision = self.name_revision.wrapping_add(1);
                    }
                    step.events.push(Event::Session {
                        id: id.into(),
                        title: renamed.or_else(|| thread_name(&thread["name"])),
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
                        let turn = self.page(&result["initialTurnsPage"], &mut step, false)?;
                        if self.busy && self.turn.is_none() {
                            self.turn = turn;
                        }
                        // Only the initial coherent attach response may bind
                        // an active history turn. Later pages are display data.
                        if self.lifecycle_revision == 0
                            && self.notification_revision == 0
                            && self.notification_turn.is_none()
                        {
                            self.notification_turn = self.turn.clone();
                            if let Some(id) = self.notification_turn.clone() {
                                self.bind_model_turn(&id);
                            }
                        }
                    } else {
                        self.history(&mut step, None)?;
                    }
                    self.finish_setup(&mut step);
                }
                Pending::Config => {
                    let config = &result["config"];
                    self.context_config = ContextConfig {
                        // A limit scoped to what grows after the carried
                        // prefix doesn't bound the whole context the reading
                        // reports, so only the window's own point applies.
                        auto_compact: config["model_auto_compact_token_limit"]
                            .as_u64()
                            .filter(|limit| *limit > 0)
                            .filter(|_| {
                                config["model_auto_compact_token_limit_scope"]
                                    .as_str()
                                    .is_none_or(|scope| scope == "total")
                            }),
                        window: config["model_context_window"]
                            .as_u64()
                            .filter(|window| *window > 0),
                    };
                    if let Some(usage) = &mut self.usage {
                        let compact_limit = self.context_config.compact_limit(usage.context_limit);
                        if usage.compact_limit != compact_limit {
                            usage.compact_limit = compact_limit;
                            step.events.push(Event::Usage(usage.clone()));
                        }
                    }
                }
                Pending::Models => {
                    for value in array(result, "data")? {
                        let model = Model::from_codex(value).ok_or_else(|| {
                            "Unsupported Codex frame: missing string model".to_owned()
                        })?;
                        let id = model.id.clone();
                        if let Some(effort) = value["defaultReasoningEffort"].as_str() {
                            self.default_reasoning.insert(id.clone(), effort.to_owned());
                        } else {
                            self.default_reasoning.remove(&id);
                        }
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
                Pending::History {
                    earlier,
                    initial_notification_binding,
                    adoption_binding,
                } => {
                    let turn = self.page(result, &mut step, earlier)?;
                    if !earlier
                        && !self.starting_turn()
                        && adoption_binding
                            == Some((self.lifecycle_revision, self.notification_revision))
                        && self.busy
                        && self.turn.is_none()
                    {
                        self.turn = turn;
                    }
                    if initial_notification_binding
                        == Some((self.lifecycle_revision, self.notification_revision))
                        && self.notification_turn.is_none()
                        && self.busy
                    {
                        self.notification_turn = self.turn.clone();
                        if let Some(id) = self.notification_turn.clone() {
                            self.bind_model_turn(&id);
                        }
                    }
                    self.finish_history(&mut step)?;
                }
                Pending::FastMode {
                    enabled,
                    revision,
                    model,
                    reasoning,
                } => {
                    // A settings notification can confirm or supersede this
                    // request before its acknowledgement arrives.
                    if revision == self.service_tier_revision {
                        self.service_tier = enabled.then(|| "fast".into());
                        if model.is_some() {
                            self.effective_model = model;
                            self.effective_reasoning = reasoning;
                            if !self.busy {
                                self.model_turn = None;
                                self.running_model = self.effective_model.clone();
                                self.running_reasoning = self.effective_reasoning.clone();
                                self.active_model_event(&mut step);
                            }
                        }
                        self.service_tier_revision = self.service_tier_revision.saturating_add(1);
                        self.fast_mode_event(&mut step);
                        step.events.push(Event::Capabilities(self.capabilities()));
                    }
                }
                Pending::Turn {
                    revision,
                    notification_revision,
                    model,
                    reasoning,
                    tier,
                    mut early,
                    ..
                } => {
                    self.materialized = true;
                    let id = string(&result["turn"], "id")?;
                    let owns_current = notification_revision == self.notification_revision
                        || self.notification_turn.as_deref() == Some(id);
                    let early_completion = early.remove(id);
                    // The answer names the very turn a stop given while it
                    // was unanswered was for, whichever lifecycle it now
                    // belongs to. One that already finished needs none.
                    if self.held_stop == HeldStop::Waiting {
                        if early_completion
                            .as_ref()
                            .is_some_and(|early| early.completed.is_some())
                        {
                            self.held_stop = HeldStop::Dropped;
                        } else {
                            self.held_stop = HeldStop::Sent;
                            self.call(
                                &mut step,
                                "turn/interrupt",
                                json!({"threadId":self.thread,"turnId":id}),
                                Pending::Other,
                            )?;
                        }
                    }
                    // An active-status update may announce newer work before
                    // its turn ID arrives. The old request's correlation alone
                    // cannot assign that lifecycle to its acknowledged turn.
                    let owns_lifecycle = owns_current
                        && (revision == self.lifecycle_revision
                            || self.notification_turn.as_deref() == Some(id)
                            // Scalar idle can follow this early completion.
                            // It announces no newer work to protect from replay.
                            || (!self.busy
                                && !early_completion.as_ref().is_some_and(|entry| entry.superseded)));
                    let early_completion = early_completion.filter(|_| owns_lifecycle);
                    // A completion can beat its ACK too. Confirming that
                    // exact last turn fills its model without reviving work.
                    let owns_model = owns_lifecycle
                        || (self.ready && self.attached && self.model_turn.as_deref() == Some(id));
                    if owns_model {
                        self.running_model = model.clone().or_else(|| self.effective_model.clone());
                        self.running_reasoning = reasoning;
                        self.bind_model_turn(id);
                        self.active_model_event(&mut step);
                    }
                    // Even a late reply identifies its turn. Remember it so
                    // a delayed start cannot replace a newer turn's status.
                    self.remember_model_turn(id);
                    self.remember_turn_pricing(id, model, tier);
                    // Notifications can finish this turn or start another before
                    // its RPC reply. An acknowledgement only fills missing state.
                    if owns_lifecycle {
                        self.notification_turn = Some(id.to_owned());
                    }
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
                    let execution_completion = if let Some(execution) = self.execution.as_mut()
                        && frame["id"].as_str() == Some(execution.request.as_str())
                    {
                        let turn = string(&result["turn"], "id")?.to_owned();
                        execution.turn = Some(turn.clone());
                        let completed = execution.early.remove(&turn);
                        execution.early.clear();
                        execution.early_bytes = 0;
                        completed
                    } else {
                        None
                    };
                    if let Some(early) = early_completion {
                        if !early.preview_transferred {
                            step.notification_preview = Some(early.preview);
                        }
                        if let Some(completed) = early.completed {
                            // Preserve the original final frame's context, rather
                            // than sampling whatever the actor currently shows.
                            self.turn = Some(id.to_owned());
                            self.notification_turn = Some(id.to_owned());
                            let completion = self.receive(json!({"method":"turn/completed",
                            "params":{"threadId":self.thread,"turn":completed}}))?;
                            step.events.extend(completion.events);
                            step.writes.extend(completion.writes);
                        }
                    } else if !owns_lifecycle {
                        step.suppress_current_notification = true;
                    }
                    if let Some(completed) = execution_completion
                        && self.execution.is_some()
                    {
                        // The replay above normally settles the exact managed
                        // execution too. Otherwise retain its outcome without
                        // changing a newer conversation lifecycle.
                        self.finish_execution(&completed, &mut step, true)?;
                        if !self.busy || self.turn.as_deref() == Some(id) {
                            self.busy = false;
                            self.turn = None;
                            step.events.push(Event::Status {
                                busy: false,
                                detail: turn_detail(&completed["status"]),
                            });
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
                                // The listing may have been taken before a
                                // live notification that renamed the child.
                                self.discover_child_named(
                                    id,
                                    thread,
                                    ChildName::Listed,
                                    false,
                                    &mut step,
                                )?;
                            }
                        }
                        if deferred.len() == before {
                            break;
                        }
                        remaining = deferred;
                    }
                }
                Pending::ChildResume(id) => {
                    self.resume_waiting_children(&id, &mut step)?;
                    let exact = result["thread"]["id"].as_str() == Some(id.as_str());
                    if exact {
                        self.read_child_name(&id, &result["thread"], &mut step);
                    }
                    // Overflow abandons only hydration, retaining live content.
                    if !self.child_buffer.contains_key(&id) {
                        return Ok(step);
                    }
                    if !exact {
                        return Err("Codex returned a different child thread".into());
                    }
                    if let Some(child) = self.children.get_mut(&id) {
                        let mut content = Step::default();
                        child.selected = result["model"]
                            .as_str()
                            .filter(|model| !model.is_empty())
                            .map(str::to_owned)
                            .or_else(|| child.selected.clone());
                        child.thread_state(&result["thread"], true, &mut content)?;
                        child.ready = true;
                        child.materialized = true;
                        if result["initialTurnsPage"].is_object() {
                            let turn =
                                child.page(&result["initialTurnsPage"], &mut content, false)?;
                            if child.busy && child.turn.is_none() {
                                child.turn = turn;
                            }
                            // This is the child's coherent initial attachment;
                            // buffered live frames have not been replayed yet.
                            // Later history pages remain presentation only.
                            if child.notification_revision == 0
                                && child.notification_turn.is_none()
                                && child.busy
                            {
                                child.notification_turn = child.turn.clone();
                                if let Some(id) = child.notification_turn.clone() {
                                    child.bind_model_turn(&id);
                                }
                            }
                        } else {
                            for turn in result["thread"]["turns"].as_array().into_iter().flatten() {
                                // A turn still under way leaves its work running.
                                let done = turn["status"] != "inProgress";
                                for value in turn["items"].as_array().into_iter().flatten() {
                                    for item in parse_item(value, done, string(turn, "id")?)? {
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
                Pending::ReadThread { name_revision } => {
                    if result["thread"].is_object() {
                        self.thread_state(
                            &result["thread"],
                            name_revision == self.name_revision,
                            &mut step,
                        )?;
                    }
                }
                Pending::Other => {
                    if result["thread"].is_object() {
                        self.thread_state(&result["thread"], true, &mut step)?;
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
            let parent = parent_thread(metadata);
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
        let notification_frame_turn = params["turnId"]
            .as_str()
            .or_else(|| params["turn"]["id"].as_str())
            .map(str::to_owned);
        if method != "turn/started"
            && notification_frame_turn
                .as_deref()
                .is_some_and(|turn| self.notification_turn.as_deref() != Some(turn))
        {
            step.suppress_current_notification = true;
        }
        match method {
            "thread/started" => {
                if self.ready {
                    self.thread_state(&params["thread"], self.name_revision == 0, &mut step)?;
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
            "thread/name/updated" => {
                if let Some(id) = params["threadId"].as_str()
                    && self.thread.as_deref() == Some(id)
                {
                    let title = thread_name(&params["threadName"]);
                    if self.attached || self.is_child {
                        if title.is_some() {
                            self.name_revision = self.name_revision.wrapping_add(1);
                        }
                        step.events.push(Event::Session {
                            id: id.into(),
                            title,
                        });
                    } else if title.is_some()
                        && let Some(Pending::Attach { renamed }) = self
                            .pending
                            .values_mut()
                            .find(|pending| matches!(pending, Pending::Attach { .. }))
                    {
                        *renamed = title;
                    }
                }
            }
            "thread/settings/updated" => {
                let settings = &params["threadSettings"];
                if settings.get("model").is_some() {
                    self.effective_model = settings["model"].as_str().map(str::to_owned);
                }
                if settings.get("effort").is_some() {
                    self.effective_reasoning = settings["effort"].as_str().map(str::to_owned);
                }
                if !self.busy {
                    self.model_turn = None;
                    self.running_model = self.effective_model.clone();
                    self.running_reasoning = self.effective_reasoning.clone();
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
                }
                self.model_events(&mut step);
            }
            "turn/started" | "turn/completed" => {
                let turn = &params["turn"];
                let turn_id = string(turn, "id")?;
                if method == "turn/started" {
                    let same_model_turn = self.model_turn.as_deref() == Some(turn_id);
                    if (self.recent_model_turns.iter().any(|id| id == turn_id) && !same_model_turn)
                        || (same_model_turn
                            && (self.model_turn_settled
                                || (!self.busy
                                    && self.notification_turn.as_deref() != Some(turn_id))))
                    {
                        step.suppress_current_notification = true;
                        return Ok(step);
                    }
                    if !same_model_turn {
                        // A start has no request ID. An outstanding override
                        // cannot be attributed until its exact ACK arrives.
                        let awaiting_ack = self
                            .pending
                            .values()
                            .any(|pending| matches!(pending, Pending::Turn { .. }));
                        self.running_model = (!awaiting_ack)
                            .then(|| self.effective_model.clone())
                            .flatten();
                        self.running_reasoning = (!awaiting_ack)
                            .then(|| self.effective_reasoning.clone())
                            .flatten();
                        self.bind_model_turn(turn_id);
                    }
                }
                // A completed ID is current only when a start, active attach,
                // or exact accepted request reply has established ownership.
                let current =
                    method == "turn/started" || self.notification_turn.as_deref() == Some(turn_id);
                if !current {
                    step.suppress_current_notification = true;
                    if let Some(Pending::Turn { early, early_bytes, .. }) =
                        self.pending.values_mut().find(|pending| matches!(pending,
                            Pending::Turn { notification_revision, .. } if *notification_revision == self.notification_revision))
                    {
                        let bytes = serde_json::to_vec(turn).map_err(|error| error.to_string())?.len();
                        let previous = early.get(turn_id).and_then(|entry| entry.completed.as_ref()).map(serde_json::to_vec)
                            .transpose().map_err(|error| error.to_string())?.map_or(0, |value| value.len());
                        let next_bytes = early_bytes.saturating_sub(previous).saturating_add(bytes);
                        if (!early.contains_key(turn_id) && early.len() >= 8)
                            || next_bytes > crate::agent_execution::MAX_RESULT_BYTES
                        {
                            return Err("Codex notification completion exceeded pending request correlation capacity".into());
                        }
                        *early_bytes = next_bytes;
                        early.entry(turn_id.to_owned()).or_default().completed = Some(turn.clone());
                    }
                }
                if method == "turn/started" {
                    // The observed start now proves the early stream's exact
                    // owner. Transfer it once, before later live deltas, and
                    // consume the saved preview so the RPC ACK cannot rewind
                    // it. Retain any known terminal proof for that exact ACK.
                    if let Some(Pending::Turn { early, .. }) = self.pending.values_mut().find(|pending|
                        matches!(pending, Pending::Turn { notification_revision, .. } if *notification_revision == self.notification_revision))
                        && let Some(context) = early.get_mut(turn_id) {
                            step.notification_preview = Some(std::mem::take(&mut context.preview).at_observed_start());
                            context.preview_transferred = true;
                        }
                    self.notification_revision = self.notification_revision.wrapping_add(1);
                    self.notification_turn = Some(turn_id.to_owned());
                    self.suggestion_turn = None;
                    step.events.push(Event::TurnStarted);
                    step.events
                        .push(Event::PromptCache(chat::prompt_cache::Evidence::Request));
                }
                self.materialized = true;
                if method == "turn/completed" && current {
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
                if method == "turn/started" {
                    let (model, _reasoning, tier) = self
                        .pending
                        .values()
                        .find_map(|pending| match pending {
                            Pending::Turn {
                                model,
                                reasoning,
                                tier,
                                ..
                            } => Some((model.clone(), reasoning.clone(), tier.clone())),
                            _ => None,
                        })
                        .unwrap_or_else(|| {
                            (
                                self.effective_model.clone(),
                                self.effective_reasoning.clone(),
                                self.service_tier.clone(),
                            )
                        });
                    self.remember_turn_pricing(turn_id, model, tier);
                    self.prediction_turn = Some(turn_id.into());
                }
                let prediction_turn =
                    method == "turn/completed" && self.prediction_turn.as_deref() == Some(turn_id);
                if prediction_turn {
                    self.prediction_turn = None;
                }
                if current {
                    if method == "turn/completed" {
                        if self.model_turn.as_deref() == Some(turn_id) {
                            self.model_turn_settled = true;
                        }
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
                    self.model_events(&mut step);
                }
                for value in array(turn, "items")? {
                    self.note_file_change(turn_id, value, &mut step);
                    let mut windows = BTreeMap::new();
                    let parsed = parse_item_with_windows(
                        value,
                        method == "turn/completed",
                        turn_id,
                        |id, window| {
                            windows.insert(id.to_owned(), window);
                        },
                    )?;
                    for item in parsed {
                        if method == "turn/started" {
                            self.track_snapshot_item(value, &item, windows.remove(&item.id), true);
                        }
                        step.events.push(Event::Item(item));
                    }
                }
                // Where the person stopped the turn, after the work it shows,
                // as its history shows it.
                if method == "turn/completed" && current && turn["status"] == "interrupted" {
                    step.events.push(Event::Item(stopped_marker(turn_id)));
                }
                // Its diff can no longer change, and its row says all of it.
                if method == "turn/completed" {
                    self.turn_diffs.remove(turn_id);
                }
                // Non-blocking questions can outlive their originating turn.
                // Only serverRequest/resolved or thread closure invalidates them.
                if current {
                    if !self.busy {
                        // A call the turn ended without finishing, as a stop
                        // leaves one, ran no further than the turn did.
                        for mut item in std::mem::take(&mut self.active_items).into_values() {
                            if let Some(status @ ToolStatus::Running) = item.content.status_mut() {
                                *status = ToolStatus::Stopped;
                                step.events.push(Event::Item(item));
                            }
                        }
                        self.output_streams.clear();
                        self.output_windows.clear();
                    }
                    step.events.push(Event::Status {
                        busy: self.busy,
                        detail: turn_detail(&turn["status"]),
                    });
                }
                if turn["error"].is_object() {
                    let message = turn["error"]["message"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| turn["error"].to_string());
                    // Codex repeats a child's final error notification in the
                    // turn's failed completion; one failure keeps one item.
                    let reported = self
                        .child_failure
                        .as_ref()
                        .is_some_and(|(turn, text)| turn == turn_id && *text == message);
                    if self.is_child && !reported {
                        step.events.push(provider_error_notice(message));
                    } else if !self.is_child {
                        step.events.push(Event::Error {
                            message,
                            fatal: false,
                        });
                    }
                }
                if method == "turn/completed" {
                    self.finish_execution(turn, &mut step, false)?;
                    if current {
                        self.notification_revision = self.notification_revision.wrapping_add(1);
                        self.notification_turn = None;
                    }
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
                let mut windows = BTreeMap::new();
                let parsed = parse_item_with_windows(
                    &params["item"],
                    done,
                    string(params, "turnId")?,
                    |id, window| {
                        windows.insert(id.to_owned(), window);
                    },
                )?;
                if done {
                    self.note_file_change(string(params, "turnId")?, &params["item"], &mut step);
                }
                for item in parsed {
                    if done {
                        self.active_items.remove(&item.id);
                        self.output_streams.remove(&item.id);
                        self.output_windows.remove(&item.id);
                    } else {
                        self.track_snapshot_item(
                            &params["item"],
                            &item,
                            windows.remove(&item.id),
                            true,
                        );
                    }
                    step.events.push(Event::Item(item));
                }
                // Codex sends a tool's result back in a new request. Hosted
                // tools run inside the request that called them. Codex reports
                // a request's usage only after its tools finish, so the reading
                // that follows may belong to an earlier request.
                if done
                    && matches!(
                        params["item"]["type"].as_str(),
                        Some(
                            "commandExecution"
                                | "fileChange"
                                | "mcpToolCall"
                                | "dynamicToolCall"
                                | "collabAgentToolCall"
                                | "imageView"
                        )
                    )
                {
                    step.events
                        .push(Event::PromptCache(chat::prompt_cache::Evidence::Continued));
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
                    base
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
                if let Some(item) = self.active_items.get_mut(&id)
                    && let ItemContent::Changes { changes, .. } = &mut item.content
                {
                    *changes = file_changes(params)?;
                    step.events.push(Event::Item(item.clone()));
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
                // Codex repeats its latest reading when a request fails, such
                // as on a usage limit; only a grown total is a new request's.
                // `inputTokens` includes what the cache served.
                let total = usage["total"]["inputTokens"].as_u64().unwrap_or(0);
                if total > std::mem::replace(&mut self.cache_reading_total, total) {
                    step.events
                        .push(Event::PromptCache(chat::prompt_cache::Evidence::Read {
                            prompt: usage["last"]["inputTokens"].as_u64().unwrap_or(0),
                            cached: usage["last"]["cachedInputTokens"].as_u64().unwrap_or(0),
                        }));
                }
                let context_limit = usage["modelContextWindow"].as_u64().unwrap_or(0);
                let reading = Usage {
                    cost_estimate,
                    input_tokens: usage["total"]["inputTokens"].as_u64().unwrap_or(0),
                    output_tokens: usage["total"]["outputTokens"].as_u64().unwrap_or(0),
                    context_tokens: usage["last"]["totalTokens"].as_u64().unwrap_or(0),
                    context_limit,
                    compact_limit: self.context_config.compact_limit(context_limit),
                    cost_microusd: None,
                };
                self.usage = Some(reading.clone());
                step.events.push(Event::Usage(reading));
            }
            "thread/closed" | "thread/deleted" => {
                // Close revokes current notification ownership. Pending exact
                // managed outcomes may still settle independently at their ACK.
                self.notification_revision = self.notification_revision.wrapping_add(1);
                self.notification_turn = None;
                self.ready = false;
                self.attached = false;
                self.subagent_waiting = false;
                self.subagent_detail = "Agent disconnected".into();
                step.events.push(Event::Ready(false));
                let interrupted = self.busy;
                self.busy = false;
                self.subagent_status = Some(
                    self.subagent_status
                        .filter(|status| *status != ToolStatus::Running)
                        .unwrap_or(ToolStatus::Stopped),
                );
                self.lifecycle_revision = self.lifecycle_revision.saturating_add(1);
                self.invalidate(&mut step);
                if !self.is_child {
                    step.events.push(Event::Error {
                        message: format!("Codex {method}; reconnect explicitly"),
                        fatal: true,
                    });
                } else if interrupted {
                    // Codex also closes a child's thread once its work is
                    // done; only a close that cut work short is news, and the
                    // child has no reconnect of its own to point at.
                    step.events.push(session_notice(
                        if method == "thread/deleted" {
                            "Codex removed this agent before it finished"
                        } else {
                            "Codex closed this agent before it finished"
                        }
                        .into(),
                    ));
                }
            }
            "error" => {
                let message = params["error"]["message"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| params["error"].to_string());
                // A retry describes work still in progress. Putting it in
                // Snapshot.error leaves a recovered turn marked as failed.
                // Child diagnostics must likewise stay in their own timeline
                // before generic child routing discards parent error state.
                if params["willRetry"] == true || self.is_child {
                    if self.is_child
                        && params["willRetry"] != true
                        && let Some(turn) = params["turnId"].as_str()
                    {
                        self.child_failure = Some((turn.to_owned(), message.clone()));
                    }
                    step.events.push(provider_error_notice(message));
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
                    .unwrap_or_else(|| params.to_string());
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
            // The turn's cumulative diff restates the file changes its own
            // items show; only the files none of them edited are its own.
            "turn/diff/updated" => {
                let turn = string(params, "turnId")?;
                self.turn_diff(turn).diff = string(params, "diff")?.into();
                self.show_turn_diff(turn, &mut step);
            }
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
        if step.suppress_current_notification
            && let Some(turn) = notification_frame_turn
                && let Some(Pending::Turn { early, .. }) = self.pending.values_mut().find(|pending|
                    matches!(pending, Pending::Turn { notification_revision, .. } if *notification_revision == self.notification_revision))
                {
                    if !early.contains_key(&turn) && early.len() >= 8 {
                        return Err("Codex notification context exceeded pending request correlation capacity".into());
                    }
                    let context = early.entry(turn).or_default();
                    for event in &step.events { context.preview.observe(event); }
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

/// Only the provider's explicit error notification/turn field grants this
/// provenance. Client-generated disconnect or RPC diagnostics remain notices.
fn provider_error_notice(text: String) -> Event {
    let mut notice = session_notice(text);
    if let Event::Item(item) = &mut notice {
        chat::failure::mark_provider_error(crate::agent::Provider::Codex, item);
    }
    notice
}

impl Codex {
    fn observe_thread_status(&mut self, status: &Value) {
        if matches!(status["type"].as_str(), Some("active" | "systemError")) {
            for pending in self.pending.values_mut() {
                if let Pending::Turn { early, .. } = pending {
                    for entry in early.values_mut().filter(|entry| entry.completed.is_some()) {
                        entry.superseded = true;
                    }
                }
            }
        }
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
    fn thread_state(
        &mut self,
        thread: &Value,
        read_name: bool,
        step: &mut Step,
    ) -> Result<(), String> {
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
            title: read_name.then(|| thread_name(&thread["name"])).flatten(),
        });
        step.events.push(Event::Status {
            busy: self.busy,
            detail: status_detail(&thread["status"]),
        });
        step.events.push(Event::Capabilities(self.capabilities()));
        Ok(())
    }
}

fn thread_name(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
}
/// The thread that spawned a subagent's thread, as its metadata names it.
fn parent_thread(metadata: &Value) -> Option<&str> {
    metadata["parentThreadId"]
        .as_str()
        .or_else(|| metadata["source"]["subAgent"]["thread_spawn"]["parent_thread_id"].as_str())
}

/// The nickname Codex reported for a child, if it reported a real one.
fn agent_nickname(metadata: &Value) -> Option<&str> {
    metadata["agentNickname"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

fn agent_status(status: &Value) -> Option<ToolStatus> {
    match status.as_str().or_else(|| status["type"].as_str()) {
        // A thread the server hasn't loaded runs nothing: it is as finished
        // as an idle one, whose last turn its own read then reports.
        Some("completed" | "idle" | "notLoaded" | "shutdown") => Some(ToolStatus::Succeeded),
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

/// Where the person stopped turn `turn`: one marker, whether the turn is
/// seen ending or read back from history.
fn stopped_marker(turn: &str) -> Item {
    crate::chat::stopped_notice(format!("codex-stopped:{turn}"))
}

/// A turn status as a line a reader can act on.
///
/// The successful states are silent: an in-progress turn is already drawn as
/// activity and a completed one is drawn by its answer, so naming either one
/// only spends a row of the status line on a word the reader can see.
fn turn_detail(status: &Value) -> String {
    match status.as_str() {
        Some("interrupted") => chat::TURN_INTERRUPTED.into(),
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
    // A decision with terms is an object keyed by its name.
    let name = value.as_str().unwrap_or_else(|| {
        value
            .as_object()
            .filter(|decision| decision.len() == 1)
            .and_then(|decision| decision.keys().next())
            .map_or(id, String::as_str)
    });
    match name {
        "accept" => "Allow once",
        "acceptForSession" => "Allow for session",
        "decline" | "deny" => "Deny",
        "cancel" => "Deny and cancel",
        "grant-turn" => "Grant for this turn",
        "grant-session" => "Grant for session",
        "submit" => "Submit answers",
        "acceptWithExecpolicyAmendment" => "Allow and remember",
        // A decision this version doesn't know is named by its own words:
        // `{"acceptWithNetworkPolicy": …}` is offered as Accept with network
        // policy.
        other => return words(other),
    }
    .into()
}
/// A camelCase protocol name as words: `acceptForSession` reads as Accept
/// for session.
fn words(name: &str) -> String {
    let mut words = String::with_capacity(name.len() + 4);
    for (index, c) in name.chars().enumerate() {
        if index == 0 {
            words.extend(c.to_uppercase());
        } else if c.is_uppercase() {
            words.push(' ');
            words.extend(c.to_lowercase());
        } else {
            words.push(c);
        }
    }
    words
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
/// Codex's own best-effort parse of a shell command, which decomposes a
/// compound command into an action for each part it kept. A command is a
/// read, a search or a listing only when that is all of it: every part the
/// same kind of action on the same target. `cat a.md; cat b.md`, `ls; rg x`
/// and anything with an `unknown` part run several things, which a label for
/// one of them would misreport, so they are commands the view names by what
/// ran. The label is what the action was applied to; the view supplies the
/// verb, from the action.
fn command_intent(actions: &Value) -> (CommandAction, String) {
    let unclassified = (CommandAction::Run, String::new());
    let Some(actions) = actions.as_array() else {
        return unclassified;
    };
    let mut intents = actions.iter().map(|action| {
        let path = action["path"].as_str();
        match action["type"].as_str() {
            Some("read") => Some((
                CommandAction::Read,
                path.map(str::to_owned).unwrap_or_default(),
            )),
            Some("search") => Some((
                CommandAction::Search,
                match (action["query"].as_str(), path) {
                    (Some(query), Some(path)) => format!("{query} in {path}"),
                    (Some(query), None) => query.to_owned(),
                    (None, Some(path)) => path.to_owned(),
                    (None, None) => String::new(),
                },
            )),
            Some("listFiles") => Some((
                CommandAction::List,
                path.map(str::to_owned).unwrap_or_default(),
            )),
            _ => None,
        }
    });
    let Some(Some(first)) = intents.next() else {
        return unclassified;
    };
    if intents.all(|intent| intent.as_ref() == Some(&first)) {
        first
    } else {
        unclassified
    }
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
            lines: crate::chat::LineOrigin::File,
            path,
            kind,
            // Reporting an absent patch as an empty one would render a real
            // edit as a change that touched nothing.
            diff: string(change, "diff")?.into(),
        });
    }
    Ok(changes)
}
/// The lines a file change added and removed, as `+line` and `-line`. A
/// patch is read from its hunks; a change Codex gives as the file's content,
/// as it may for a file it added or deleted, is every line of it.
fn changed_lines(diff: &str, kind: Option<&str>) -> Vec<String> {
    let hunks = if diff.starts_with("@@") {
        Some(0)
    } else {
        diff.find("\n@@").map(|at| at + 1)
    };
    match hunks {
        Some(start) => diff[start..]
            .lines()
            .filter(|line| line.starts_with('+') || line.starts_with('-'))
            .map(str::to_owned)
            .collect(),
        _ => {
            let sign = match kind {
                Some("add") => '+',
                Some("delete") => '-',
                _ => return Vec::new(),
            };
            diff.lines().map(|line| format!("{sign}{line}")).collect()
        }
    }
}

/// The files a turn's cumulative diff changed, one per `diff --git` section,
/// each with its own hunks.
fn turn_diff_changes(diff: &str) -> Vec<FileChange> {
    let mut sections = Vec::new();
    let mut start = None;
    let mut offset = 0;
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            if let Some(start) = start {
                sections.push(&diff[start..offset]);
            }
            start = Some(offset);
        }
        offset += line.len();
    }
    if let Some(start) = start {
        sections.push(&diff[start..]);
    }
    sections
        .into_iter()
        .filter_map(|section| {
            let header_end = section.find("\n@@").map_or(section.len(), |at| at + 1);
            let header = &section[..header_end];
            let field = |prefix: &str| {
                header
                    .lines()
                    .find_map(|line| line.strip_prefix(prefix))
                    .map(str::trim_end)
            };
            let side = |prefix: &str, marker: &str| {
                field(prefix)
                    .filter(|path| *path != "/dev/null")
                    .map(|path| path.strip_prefix(marker).unwrap_or(path).to_owned())
            };
            let old = side("--- ", "a/");
            let new = side("+++ ", "b/");
            // A change with no hunks, such as an empty or binary file or a
            // mode change, names its path only on its first line.
            let named = || {
                let first = header.lines().next()?.strip_prefix("diff --git a/")?;
                first.split_once(" b/").map(|(_, path)| path.to_owned())
            };
            let (path, kind) =
                if let (Some(from), Some(to)) = (field("rename from "), field("rename to ")) {
                    (to.to_owned(), ChangeKind::Renamed { from: from.into() })
                } else if header.contains("\nnew file mode") {
                    (new.or(old).or_else(named)?, ChangeKind::Added)
                } else if header.contains("\ndeleted file mode") {
                    (old.or(new).or_else(named)?, ChangeKind::Deleted)
                } else {
                    (new.or(old).or_else(named)?, ChangeKind::Updated)
                };
            let body = &section[header_end..];
            Some(FileChange {
                lines: crate::chat::LineOrigin::File,
                path,
                kind,
                // A change with no hunks keeps what its header says of it,
                // and one with hunks any change of mode it also made.
                diff: if body.is_empty() {
                    header
                        .lines()
                        .skip(1)
                        .filter(|line| !line.starts_with("index "))
                        .map(|line| format!("{line}\n"))
                        .collect()
                } else {
                    header
                        .lines()
                        .filter(|line| {
                            line.starts_with("old mode ") || line.starts_with("new mode ")
                        })
                        .map(|line| format!("{line}\n"))
                        .chain(std::iter::once(body.to_owned()))
                        .collect()
                },
            })
        })
        .collect()
}

/// The agents a collaboration call addressed, each with its state when Codex
/// reported one. A call in progress, or one its turn cut short, names its
/// receivers before it has their states, and the row still says whom it was
/// for.
fn agent_states(value: &Value) -> Value {
    let mut states = value["agentsStates"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    for receiver in value["receiverThreadIds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(32)
    {
        states
            .entry(receiver)
            .or_insert_with(|| Value::Object(Default::default()));
    }
    Value::Object(states)
}

/// A tool call's arguments as its detail shows them: nothing when it was
/// called with none.
fn arguments(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Object(object) if object.is_empty() => String::new(),
        Value::String(text) => text.clone(),
        value => value.to_string(),
    }
}
fn parse_item(value: &Value, done: bool, turn: &str) -> Result<Vec<Item>, String> {
    parse_item_with_windows(value, done, turn, |_, _| {})
}
fn parse_item_with_windows(
    value: &Value,
    done: bool,
    turn: &str,
    mut parsed_window: impl FnMut(&str, OutputWindow),
) -> Result<Vec<Item>, String> {
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
        // A collaboration call cut short with its turn.
        Some("interrupted") => ToolStatus::Stopped,
        // Still in progress when its item or turn ended: the turn ended it
        // unfinished, as a stop does, and nothing will finish it later.
        Some("inProgress") if done => ToolStatus::Stopped,
        Some("inProgress") => ToolStatus::Running,
        Some("completed") => ToolStatus::Succeeded,
        _ if done => ToolStatus::Succeeded,
        _ => ToolStatus::Running,
    };
    let tool_call = |name: String, input: String, output: String| {
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
            let contents = array(value, "content")?;
            let mut synthetic = std::collections::HashSet::new();
            let mut references = std::collections::HashMap::new();
            let mut unverified = std::collections::HashMap::new();
            for (index, pair) in contents.windows(2).enumerate() {
                if pair[0]["type"] == "text"
                    && matches!(pair[1]["type"].as_str(), Some("localImage" | "image"))
                    && let (Some(marker), Some(source)) = (
                        pair[0]["text"].as_str(),
                        pair[1]["path"].as_str().or_else(|| pair[1]["url"].as_str()),
                    )
                    && super::image_references::claims(marker, source)
                {
                    if let Some(reference) = super::image_references::restore(marker, source) {
                        synthetic.insert(index);
                        references.insert(index + 1, reference);
                    } else if !source.starts_with("data:image/") {
                        // Keep the literal marker visible, but do not read a
                        // possibly foreign device's coincident absolute path.
                        unverified.insert(index + 1, format!("{} is unavailable on this device: its owned image receipt could not be verified. Use the originating device or attach the image again.", super::image_references::claimed_label(marker, source)));
                    }
                }
            }
            for (index, content) in contents.iter().enumerate() {
                if synthetic.contains(&index) {
                    continue;
                }
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
                            reference: references.get(&index).cloned(),
                            path: if unverified.contains_key(&index) {
                                String::new()
                            } else {
                                content["path"]
                                    .as_str()
                                    .or_else(|| content["url"].as_str())
                                    .ok_or("Missing image path")?
                                    .into()
                            },
                            description: unverified.get(&index).cloned().unwrap_or_else(|| {
                                references
                                    .get(&index)
                                    .map(|reference| reference.label())
                                    .unwrap_or_else(|| "User attachment".into())
                            }),
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
        // Named by the app the call went through when Codex knows it, as the
        // person connected it, and otherwise by its server; the reader's
        // `server / tool` form is the one the transcript titles.
        "mcpToolCall" => {
            let app = &value["appContext"];
            let server = app["appName"]
                .as_str()
                .filter(|name| !name.trim().is_empty())
                .map_or_else(|| string(value, "server"), Ok)?;
            let tool = app["actionName"]
                .as_str()
                .filter(|name| !name.trim().is_empty())
                .map_or_else(|| string(value, "tool"), Ok)?;
            match value["error"]["message"].as_str() {
                // What went wrong, as the server said it, rather than the
                // error object it was carried in.
                Some(message) => tool_call(
                    format!("{server} / {tool}"),
                    arguments(&value["arguments"]),
                    message.into(),
                ),
                None => images::tool_result(
                    id,
                    status,
                    format!("{server} / {tool}"),
                    arguments(&value["arguments"]),
                    if !value["error"].is_null() {
                        &value["error"]
                    } else {
                        &value["result"]
                    },
                ),
            }
        }
        "dynamicToolCall" => images::tool_result(
            id,
            status,
            string(value, "tool")?.into(),
            arguments(&value["arguments"]),
            &value["contentItems"],
        ),
        "collabAgentToolCall" => tool_call(
            string(value, "tool")?.into(),
            value["prompt"].as_str().unwrap_or("").into(),
            agent_states(value).to_string(),
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
        "functionCallOutput" => tool_call(
            string(value, "name")?.into(),
            String::new(),
            images::readable(&value["output"]),
        ),
        "imageView" => vec![Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Image {
                reference: None,
                path: string(value, "path")?.into(),
                // The file it looked at, which is what a reader would call it.
                description: std::path::Path::new(string(value, "path")?)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("Image")
                    .into(),
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
                            reference: None,
                            path,
                            description: "Generated image".into(),
                        },
                    }]
                }
                _ => tool_call(
                    "Image generation".into(),
                    value["revisedPrompt"].as_str().unwrap_or("").into(),
                    match value["failure"]["type"].as_str() {
                        Some("usageLimitExceeded") => {
                            "Image generation has reached its usage limit.".into()
                        }
                        Some(other) => format!("Image generation failed ({other})."),
                        // Still generating: there is nothing to say yet.
                        None if status == ToolStatus::Running => String::new(),
                        None => "No displayable image was returned.".into(),
                    },
                ),
            }
        }
        "enteredReviewMode" | "exitedReviewMode" => vec![text(
            id.into(),
            string(value, "review")?.into(),
            false,
            Role::System,
        )],
        // Codex starts the item when it begins compacting and completes it
        // when the context has been replaced. The name stays the same either
        // way and the status says how far it got, so an item a turn left
        // unfinished, or one replayed from a turn still running, never reads
        // as still compacting once it is not.
        "contextCompaction" => tool_call("Compact context".into(), String::new(), String::new()),
        // Context the hooks added for the model, in their own words, as a
        // step among the turn's work rather than as the agent's prose or a
        // notice; the run IDs the fragments are keyed by are Codex's
        // bookkeeping. Hooks that added nothing leave no row.
        "hookPrompt" => {
            let added = value["fragments"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|fragment| fragment["text"].as_str())
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            if added.is_empty() {
                Vec::new()
            } else {
                tool_call(super::HOOK_CONTEXT.into(), String::new(), added)
            }
        }
        // The agent pausing, named as a pause. As with compaction, the status
        // and not the wording says whether the pause is over.
        "sleep" => {
            let seconds = value["durationMs"].as_u64().unwrap_or(0).div_ceil(1000);
            let duration = match seconds {
                0..60 => format!("{seconds}s"),
                _ => format!("{}m {}s", seconds / 60, seconds % 60),
            };
            tool_call(format!("Wait {duration}"), String::new(), String::new())
        }
        // A subagent's lifecycle is reported on the subagent itself, as its
        // collaboration calls and thread status discover it; a row per
        // transition would repeat that for every agent a turn coordinates.
        "subAgentActivity" => Vec::new(),
        // Any other auxiliary item, and any item type a later Codex
        // introduces, still belongs to the
        // turn. The tool disclosure exists to keep an unrecognized item
        // renderable rather than ending the session over it.
        other => tool_call(other.into(), String::new(), value.to_string()),
    };
    for item in &mut items {
        let mut window = OutputWindow::default();
        let bytes = item.content.byte_len();
        let limit = if matches!(&item.content, ItemContent::Image { path, .. } if path.starts_with("data:image/"))
        {
            chat::MAX_INLINE_IMAGE_URL_BYTES + 1024
        } else {
            MAX_ITEM_BYTES
        };
        if bytes > MAX_ITEM_BYTES
            && let ItemContent::Tool { output, .. } | ItemContent::Command { output, .. } =
                &mut item.content
        {
            let limit = MAX_ITEM_BYTES.saturating_sub(bytes.saturating_sub(output.len()));
            // Completed items replace streamed items, so apply the same bound.
            let full = std::mem::take(output);
            append_display_output(output, &full, limit, &mut window);
        }
        if item.content.byte_len() > limit {
            return Err("Codex item exceeds native client size bound".into());
        }
        if matches!(
            &item.content,
            ItemContent::Tool { .. } | ItemContent::Command { .. }
        ) {
            parsed_window(&item.id, window);
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
mod tests;

#[cfg(test)]
mod fresh_recipe_constructor_tests {
    use super::*;

    const SAVED: &str = "00000000-0000-4000-8000-000000000001";
    const FOREIGN: &str = "00000000-0000-4000-8000-000000000002";

    fn source(root: &std::path::Path, arguments: Vec<String>) -> chat::Session {
        let home = root.join("home");
        let config = home.join(".codex");
        let cwd = root.join("project");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        chat::Session {
            id: crate::model::TerminalId::new(),
            launch: Launch {
                provider: crate::agent::Provider::Codex,
                session_id: Some(SAVED.into()),
                recipe: crate::model::LaunchRecipe {
                    program: root.join("codex").to_string_lossy().into_owned(),
                    arguments,
                    working_directory: cwd,
                    environment: BTreeMap::from([
                        ("HOME".into(), home.to_string_lossy().into_owned()),
                        ("CODEX_HOME".into(), config.to_string_lossy().into_owned()),
                    ]),
                    unset_environment: ["OPENAI_API_KEY".into()].into(),
                },
                draft_model: None,
            },
            proof: Some(crate::model::HostSessionProof {
                host_session_id: "old-host".into(),
                boot_id: "old-boot".into(),
                host_incarnation: "old-incarnation".into(),
            }),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::Durable {
                provider_version: "0.167.0".into(),
            },
        }
    }

    #[test]
    fn fresh_recipe_constructor_admits_codex_resume_selectors_as_a_new_thread_without_reusing_authority()
     {
        let root = tempfile::tempdir().unwrap();
        for model in ["resume", "--resume"] {
            for selector in [
                vec!["resume".into(), SAVED.into()],
                vec!["resume".into(), SAVED.into(), "resume".into(), SAVED.into()],
            ] {
                let payload = vec!["--model".to_owned(), model.to_owned()];
                let saved = source(root.path(), [payload.clone(), selector.clone()].concat());
                let original = saved.clone();
                let fresh = saved
                    .fresh_start()
                    .expect("an exact thread selector can be removed for Fresh");
                assert_eq!(saved, original);
                assert_eq!(
                    fresh.id, saved.id,
                    "Fresh replaces the same pane's conversation"
                );
                assert!(fresh.launch.session_id.is_none());
                assert!(fresh.proof.is_none());
                assert!(fresh.terminal.is_none());
                assert!(fresh.owned_codex_listener.is_none());
                assert_eq!(
                    fresh.native_continuity,
                    chat::NativeContinuity::ReconnectOnly
                );
                assert_eq!(fresh.launch.recipe.arguments, payload, "{selector:?}");
                assert_eq!(
                    fresh.launch.recipe.environment,
                    saved.launch.recipe.environment
                );
                assert_eq!(
                    fresh.launch.recipe.unset_environment,
                    saved.launch.recipe.unset_environment
                );
                assert_eq!(
                    fresh.launch.recipe.working_directory,
                    saved.launch.recipe.working_directory
                );
                let socket = root.path().join("fresh.sock");
                let (spec, mut adapter) =
                    create_with_listener_for_start(&fresh.launch, socket.clone()).unwrap();
                assert!(
                    matches!(spec.transport, TransportSpec::OwnedUnixWebSocket { socket: ref actual } if actual == &socket)
                );
                let initialize = adapter.initialize_for_start().unwrap();
                let start = adapter.receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":root.path().join("home/.codex")}})).unwrap();
                let thread = start
                    .writes
                    .iter()
                    .find(|frame| frame["method"] == "thread/start")
                    .expect("Fresh must issue thread/start, never resume the saved thread");
                assert_eq!(thread["params"]["model"], model);
                assert!(
                    !start
                        .writes
                        .iter()
                        .any(|frame| frame["method"] == "thread/resume")
                );
                assert!(
                    !serde_json::to_string(&start.writes)
                        .unwrap()
                        .contains(SAVED)
                );
                assert_eq!(saved, original);
                assert!(
                    !root.path().join("codex").exists(),
                    "constructors must not execute a provider"
                );
            }
        }
    }

    #[test]
    fn fresh_recipe_constructor_rejects_foreign_or_malformed_codex_selectors_without_mutating_source()
     {
        let root = tempfile::tempdir().unwrap();
        for arguments in [
            vec!["resume".into()],
            vec!["resume".into(), "".into()],
            vec!["resume".into(), "latest".into()],
            vec!["resume".into(), FOREIGN.into()],
            vec![
                "resume".into(),
                SAVED.into(),
                "resume".into(),
                FOREIGN.into(),
            ],
            vec![
                "--model".into(),
                "resume".into(),
                "resume".into(),
                FOREIGN.into(),
            ],
        ] {
            let saved = source(root.path(), arguments.clone());
            let original = saved.clone();
            assert!(saved.fresh_start().is_none(), "{arguments:?}");
            assert_eq!(saved, original);
        }
    }

    #[test]
    fn fresh_config_admission_refuses_overrides_that_only_resume_can_ignore() {
        let root = tempfile::tempdir().unwrap();
        for flag in ["-c", "--config"] {
            for value in [
                "model=\"o3\"",
                "model_reasoning_effort=invalid token",
                "model_reasoning_effort=\"high",
            ] {
                let saved = source(root.path(), vec![flag.into(), value.into()]);
                let original = saved.clone();
                assert!(
                    create_with_listener_for_start(&saved.launch, root.path().join("saved.sock"))
                        .is_ok(),
                    "the existing thread is admitted without changing its live configuration"
                );
                assert!(saved.fresh_start().is_none(), "{flag} {value}");
                assert_eq!(saved, original);
            }
        }
        assert!(!root.path().join("codex").exists());
    }

    #[test]
    fn fresh_config_admission_preserves_quoted_and_unquoted_reasoning_effort() {
        let root = tempfile::tempdir().unwrap();
        for flag in ["-c", "--config"] {
            for (value, effort) in [
                ("model_reasoning_effort=high", "high"),
                ("model_reasoning_effort=\"low\"", "low"),
            ] {
                let saved = source(root.path(), vec![flag.into(), value.into()]);
                let original = saved.clone();
                let fresh = saved.fresh_start().unwrap();
                assert_eq!(fresh.launch.recipe.arguments, [flag, value]);
                let (_, mut adapter) =
                    create_with_listener_for_start(&fresh.launch, root.path().join("fresh.sock"))
                        .unwrap();
                let initialize = adapter.initialize_for_start().unwrap();
                let start = adapter.receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":root.path().join("home/.codex")}})).unwrap();
                let thread = start
                    .writes
                    .iter()
                    .find(|frame| frame["method"] == "thread/start")
                    .unwrap();
                assert_eq!(thread["params"]["config"][REASONING_EFFORT_KEY], effort);
                assert!(
                    !start
                        .writes
                        .iter()
                        .any(|frame| frame["method"] == "thread/resume")
                );
                assert_eq!(saved, original);
            }
        }
    }
}
