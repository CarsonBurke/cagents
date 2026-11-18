//! Claude Code's bidirectional stream-json protocol (not terminal output).
//! Wire reference: anthropics/claude-agent-sdk-python, _internal/query.py,
//! _internal/transport/subprocess_cli.py and types.py; matched SDK 0.3.236.
//! https://unpkg.com/@anthropic-ai/claude-agent-sdk@0.3.236/sdk.d.ts
use super::{Adapter, ProcessSpec, Step};
use crate::account::{self, LiveReport};
use crate::agent::Provider;
use crate::chat;
use crate::chat::{
    AgentAddress, Answer, Attachment, AttachmentRules, Capabilities, Choice, Command,
    CommandAction, DeliveryKind, Event, Field, Item, ItemContent, Launch, Model, Phase, PromptMode,
    Request, Role, Skill, SkillReach, SlashCommand, ToolStatus, Usage, clock_label,
};
use base64::Engine as _;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};
use streamed_input::StreamedInput;

/// The first Claude Code release whose async input contract (`next` and
/// `later` priorities, with lifecycle acknowledgments and removal) was
/// verified. Later releases are assumed to keep it.
const ASYNC_INPUT_SINCE: semver::Version = semver::Version::new(2, 1, 286);

/// Whether a reported `claude_code_version` is at or after
/// [`ASYNC_INPUT_SINCE`]. A version that is not semver enables nothing, and a
/// pre-release sorts before its release, which is all that was verified.
fn supports_async_input(version: &str) -> bool {
    semver::Version::parse(version).is_ok_and(|version| version >= ASYNC_INPUT_SINCE)
}
const MAX_PENDING: usize = 64;
const MAX_TOOLS: usize = 512;
const ATTACHMENTS: AttachmentRules = AttachmentRules::for_provider(Provider::Claude);
const MAX_IMAGE: u64 = ATTACHMENTS.max_image_bytes;
const MAX_DOCUMENT: u64 = ATTACHMENTS.max_file_bytes.unwrap();
const MAX_PAYLOAD: usize = 30 * 1024 * 1024;
const HISTORY_PAGE: u64 = 4 * 1024 * 1024;
/// How much of each recorded background task's summary a restored history
/// keeps; a summary quotes a command that has no description.
const RECORDED_SUMMARY_BYTES: usize = 4 * 1024;
const MAX_TEXT: usize = 1024 * 1024;
const MODES: &[&str] = &[
    "manual",
    "acceptEdits",
    "auto",
    "plan",
    "dontAsk",
    "bypassPermissions",
];

pub fn create(launch: &Launch) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    create_for_start(launch).map_err(|error| error.to_string())
}

pub(crate) fn create_for_start(
    launch: &Launch,
) -> Result<(ProcessSpec, Box<dyn Adapter>), super::SetupError> {
    create_inner(launch, None, false)
}

pub(super) fn create_history_for_start(
    launch: &Launch,
) -> Result<(ProcessSpec, Box<dyn Adapter>), super::SetupError> {
    create_inner(launch, None, true)
}

/// Whether a non-JSON stdout line carries a complete Claude frame next to other
/// text, as when a wrapper prints without a newline. Skipping that line would
/// lose the frame, and a lost `result` or `control_request` never settles. A
/// candidate must be a whole JSON object with the shape of a real frame, so
/// ordinary text that merely has braces or mentions a frame type is still noise.
pub(crate) fn embedded_protocol_frame(line: &[u8]) -> bool {
    // Inspect a bounded number of objects, however many braces a line has.
    const CANDIDATES: usize = 64;
    if !line.windows(6).any(|window| window == b"\"type\"") {
        return false;
    }
    let mut start = 0;
    for _ in 0..CANDIDATES {
        let Some(offset) = line[start..].iter().position(|byte| *byte == b'{') else {
            return false;
        };
        let at = start + offset;
        let mut values = serde_json::Deserializer::from_slice(&line[at..]).into_iter::<Value>();
        match values.next() {
            Some(Ok(value)) => {
                if has_frame_shape(&value) {
                    return true;
                }
                // Fields nested in a complete object are not top-level frames.
                start = at + values.byte_offset();
            }
            _ => start = at + 1,
        }
    }
    false
}

/// The whole record at the end of a line that opens with a damaged one, and
/// where it starts. Claude Code appends to a file a crash left mid-record,
/// so the next record shares that record's line.
fn trailing_record(line: &[u8]) -> Option<(usize, Value)> {
    let end = line.len()
        - line
            .iter()
            .rev()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
    if line.get(end.checked_sub(1)?) != Some(&b'}') {
        return None;
    }
    // A valid trailing object has balanced braces and quoted strings even
    // when everything before it is damaged. Match its opening brace from the
    // end, then parse that candidate once. No prefix can make us repeatedly
    // parse the same large suffix.
    let mut position = end;
    let mut depth = 0usize;
    let mut quoted = false;
    while position > 0 {
        position -= 1;
        match line[position] {
            b'"' => {
                let mut escapes = position;
                while escapes > 0 && line[escapes - 1] == b'\\' {
                    escapes -= 1;
                }
                if (position - escapes).is_multiple_of(2) {
                    quoted = !quoted;
                }
                // Count every escape run only once during the reverse scan.
                position = escapes;
            }
            b'}' if !quoted => depth += 1,
            b'{' if !quoted => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    if position == 0 {
                        return None;
                    }
                    let value: Value = serde_json::from_slice(&line[position..end]).ok()?;
                    return value["type"].is_string().then_some((position, value));
                }
            }
            _ => {}
        }
    }
    None
}

fn has_frame_shape(value: &Value) -> bool {
    match value["type"].as_str() {
        Some("result" | "system") => value["subtype"].is_string(),
        Some("control_request") => value["request_id"].is_string() && value["request"].is_object(),
        Some("control_cancel_request") => value["request_id"].is_string(),
        Some("control_response") => value["response"].is_object(),
        Some("assistant" | "user") => value["message"].is_object(),
        Some("stream_event") => value["event"].is_object(),
        _ => false,
    }
}

/// Transient identity selected by the original adapter during preparation.
/// A fresh UUID is not provider confirmation or a disk-resume descriptor.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PreparedIdentity {
    session_id: String,
    resumed: bool,
}
impl PreparedIdentity {
    fn validate(&self, launch: &Launch) -> Result<(), String> {
        validate_id(&self.session_id)?;
        if self.resumed != launch.session_id.is_some()
            || launch
                .session_id
                .as_deref()
                .is_some_and(|id| id != self.session_id)
        {
            return Err(
                "Retained Claude identity differs from its issued launch mode or session".into(),
            );
        }
        Ok(())
    }
}

pub(crate) fn create_prepared(
    launch: &Launch,
    identity: PreparedIdentity,
) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    identity.validate(launch)?;
    create_inner(launch, Some(identity), false).map_err(|error| error.to_string())
}

fn create_inner(
    launch: &Launch,
    prepared: Option<PreparedIdentity>,
    forced_history: bool,
) -> Result<(ProcessSpec, Box<dyn Adapter>), super::SetupError> {
    if forced_history
        && (launch
            .session_id
            .as_deref()
            .is_none_or(|id| id.len() != 36 || uuid::Uuid::parse_str(id).is_err())
            || !launch.recipe.working_directory.is_absolute()
            || launch.recipe.working_directory.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            }))
    {
        return Err(
            "Read-only history requires an exact saved conversation and absolute working directory"
                .into(),
        );
    }
    let mut spec = ProcessSpec::from(&launch.recipe);
    spec.stdout_noise = Some(embedded_protocol_frame);
    spec.arguments = arguments(launch).map_err(super::SetupError::Invalid)?;
    let history_only = forced_history || launch.needs_read_only_history();
    // A conversation whose saved history cannot be shown still resumes: the
    // provider keeps its own copy. Only a reader with no provider needs it.
    // An unresolvable launch fails here; only reading the transcript it
    // names may fall short.
    let transcript = launch
        .session_id
        .as_deref()
        .map(|id| History::locate(launch, id).map(|place| (place, id)))
        .transpose()?;
    let (history, history_error) = match transcript
        .map(|((config, cwd), id)| History::open(&config, &cwd, id))
        .transpose()
    {
        Ok(history) => (history, None),
        Err(HistoryUnavailable::Unreadable(error)) if !history_only => {
            tracing::warn!(%error, "Resuming a Claude conversation without its saved history");
            (None, Some(error.to_string()))
        }
        Err(HistoryUnavailable::Missing(error) | HistoryUnavailable::Unreadable(error)) => {
            return Err(error);
        }
    };
    if history_only {
        spec.transport = super::TransportSpec::History;
    }
    let session_id = prepared
        .map(|identity| identity.session_id)
        .or_else(|| launch.session_id.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if launch.session_id.is_none() {
        spec.arguments.push(format!("--session-id={session_id}"));
    }
    Ok((
        spec,
        Box::new(Claude {
            prepared_identity: Some(PreparedIdentity {
                session_id: session_id.clone(),
                resumed: launch.session_id.is_some(),
            }),
            session_id: Some(session_id),
            resumed: launch.session_id.is_some(),
            history_only,
            forced_history,
            turn_unsupported_content: false,
            turn_visible_content: false,
            cwd: launch.recipe.working_directory.clone(),
            history,
            history_error,
            titles: super::claude_titles::ClaudeTitles::new(launch),
            selected: launch_model(&launch.recipe.arguments),
            ..Claude::default()
        }),
    ))
}

/// The model a launch asks for with `--model`, the last one where it repeats,
/// as Claude Code reads it: until a turn reports what runs, this is the
/// conversation's model.
fn launch_model(arguments: &[String]) -> Option<String> {
    let mut model = None;
    let mut args = arguments.iter();
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--model=") {
            model = Some(value);
        } else if arg == "--model" {
            model = args.next().map(String::as_str);
        }
    }
    model.filter(|model| !model.is_empty()).map(str::to_owned)
}

/// Record a provider-confirmed rotation in the recipe: after an
/// interrupt-driven reset the live conversation differs from the originally
/// launched `--resume` ID. Rewrites the same flags `arguments()` validates
/// so a checkpointed launch stays self-consistent. Forgery checks stay in
/// `arguments()` — only call this with the live provider's own identity.
pub fn align_resume_identity(arguments: &mut Vec<String>, id: &str) -> Result<(), String> {
    let mut index = 0;
    while index < arguments.len() {
        let (flag, inline) = match arguments[index].split_once('=') {
            Some((key, _)) => (key.to_string(), true),
            None => (arguments[index].clone(), false),
        };
        if matches!(flag.as_str(), "--resume" | "-r" | "--session-id") {
            if inline {
                arguments[index] = format!("{flag}={id}");
            } else {
                index += 1;
                *arguments
                    .get_mut(index)
                    .ok_or("Claude resume requires an exact session ID")? = id.to_string();
            }
        }
        index += 1;
    }
    Ok(())
}

fn arguments(launch: &Launch) -> Result<Vec<String>, String> {
    if launch.recipe.arguments.len() > 128
        || launch
            .recipe
            .arguments
            .iter()
            .map(String::len)
            .sum::<usize>()
            > 64 * 1024
    {
        return Err("Claude launch arguments exceed the native transport limit".into());
    }
    if launch.recipe.arguments.iter().any(|arg| arg.contains('\0')) {
        return Err("Claude arguments contain a NUL byte".into());
    }
    if let Some(id) = &launch.session_id {
        validate_id(id)?;
    }
    let mut result = Vec::new();
    let mut args = launch.recipe.arguments.iter().peekable();
    while let Some(arg) = args.next() {
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
        if matches!(flag, "--resume" | "-r" | "--session-id") {
            let value = inline
                .or_else(|| args.next().map(String::as_str))
                .ok_or("Claude resume requires an exact session ID")?;
            validate_id(value)?;
            if launch.session_id.as_deref() != Some(value) {
                return Err("Claude launch identity differs from the selected session".into());
            }
        } else if matches!(
            flag,
            "--input-format" | "--output-format" | "--permission-prompt-tool"
        ) {
            let value = inline
                .or_else(|| args.next().map(String::as_str))
                .ok_or("Missing Claude transport argument value")?;
            if (flag == "--permission-prompt-tool" && value != "stdio")
                || (flag != "--permission-prompt-tool"
                    && value != "stream-json"
                    && value != "text"
                    && value != "json")
            {
                return Err(format!(
                    "Unsupported Claude transport option {flag}={value}"
                ));
            }
        } else if matches!(
            flag,
            "--print"
                | "-p"
                | "--verbose"
                | "--include-partial-messages"
                | "--replay-user-messages"
        ) && inline.is_none()
        {
            // Replaced below; never replay the positional prompt from the TUI recipe.
        } else if crate::chat::fresh::claude_arity(flag)
            == Some(crate::chat::fresh::ArgumentArity::Value)
        {
            let value = inline
                .or_else(|| args.next().map(String::as_str))
                .ok_or_else(|| format!("Missing value for Claude {flag}"))?;
            if (value.is_empty() && flag != "--setting-sources") || value.contains('\0') {
                return Err(format!("Invalid value for Claude {flag}"));
            }
            if flag == "--permission-mode"
                && !matches!(value, "default" | "manual" | "auto" | "bypassPermissions")
                && !MODES.contains(&value)
            {
                return Err(format!("Unknown Claude launch permission mode {value}"));
            }
            result.extend([flag.into(), value.into()]);
        } else if crate::chat::fresh::claude_arity(flag)
            == Some(crate::chat::fresh::ArgumentArity::Values)
        {
            result.push(flag.into());
            if let Some(value) = inline {
                result.push(value.into());
            } else {
                let start = result.len();
                while args.peek().is_some_and(|value| !value.starts_with('-')) {
                    result.push(args.next().unwrap().clone());
                }
                if result.len() == start {
                    return Err(format!("Missing value for Claude {flag}"));
                }
            }
        } else if crate::chat::fresh::claude_arity(flag)
            == Some(crate::chat::fresh::ArgumentArity::Switch)
            && inline.is_none()
        {
            result.push(arg.clone());
        } else if !flag.starts_with('-') {
            // Original one-shot prompt, intentionally not resubmitted.
        } else {
            return Err(format!(
                "Claude option {flag} cannot safely transfer to native chat; remove it before connecting"
            ));
        }
    }
    // Older stored chats and terminal conversions may predate native launch
    // defaults. Apply the account choice at the final transport boundary too.
    if !result.iter().any(|argument| {
        argument == "--permission-mode" || argument == "--dangerously-skip-permissions"
    }) && (!launch
        .recipe
        .environment
        .contains_key("CLAUDE_CODE_PERMISSION_MODE")
        || launch
            .recipe
            .unset_environment
            .contains("CLAUDE_CODE_PERMISSION_MODE"))
        && let Some(mode) = crate::chat::claude_configured_default_mode(&launch.recipe)
    {
        result.extend(["--permission-mode".into(), mode]);
    }
    result.extend(
        [
            "--print",
            "--verbose",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--replay-user-messages",
            "--permission-prompt-tool",
            "stdio",
        ]
        .map(str::to_owned),
    );
    if let Some(id) = &launch.session_id {
        result.push(format!("--resume={id}"));
    }
    Ok(result)
}

fn validate_id(id: &str) -> Result<(), String> {
    uuid::Uuid::parse_str(id).map_err(|_| {
        "Claude requires an exact UUID session ID (not a path or latest selector)".to_owned()
    })?;
    if id.len() != 36 || id.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err("Claude session ID must be a canonical lowercase UUID".into());
    }
    Ok(())
}

/// Provider IDs are opaque and local to their parent transcript. Keep the
/// namespace structural: concatenating a parent and ID can itself collide.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ScopedId {
    parent: Option<String>,
    id: String,
}

impl ScopedId {
    fn new(parent: Option<&str>, id: &str) -> Self {
        Self {
            parent: parent.filter(|id| !id.is_empty()).map(str::to_owned),
            id: id.into(),
        }
    }
}

/// The tools whose call can start a conversation of its own, which Claude
/// Code forwards under that call: an agent, by its current or former name, a
/// message that resumes one, and a skill that forks.
const SPAWNING_TOOLS: &[&str] = &["Agent", "Task", "SendMessage", "Skill"];

/// The spawning calls an assistant frame makes, whole or as a streamed
/// block, by name and call ID.
fn spawning_calls(frame: &Value) -> Vec<(&str, &str)> {
    let spawning = |block: &&Value| {
        string(block, "type") == "tool_use" && SPAWNING_TOOLS.contains(&string(block, "name"))
    };
    let blocks: Vec<&Value> = match string(frame, "type") {
        "assistant" => frame["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .collect(),
        "stream_event" if string(&frame["event"], "type") == "content_block_start" => {
            vec![&frame["event"]["content_block"]]
        }
        _ => Vec::new(),
    };
    blocks
        .into_iter()
        .filter(spawning)
        .map(|block| (string(block, "name"), string(block, "id")))
        .collect()
}

/// How many nested calls or tasks are remembered. They are agents and forked
/// skills only, so this outlasts any team's helpers by far while bounding a
/// conversation that runs for weeks.
const MAX_NESTED: usize = 8 * MAX_TOOLS;

/// IDs, oldest first, of what runs inside one of this conversation's agents.
#[derive(Default)]
struct NestedIds {
    known: HashSet<String>,
    order: VecDeque<String>,
}

impl NestedIds {
    fn contains(&self, id: &str) -> bool {
        self.known.contains(id)
    }

    fn insert(&mut self, id: &str) {
        if id.is_empty() || !self.known.insert(id.to_owned()) {
            return;
        }
        self.order.push_back(id.to_owned());
        if self.order.len() > MAX_NESTED
            && let Some(oldest) = self.order.pop_front()
        {
            self.known.remove(&oldest);
        }
    }
}

/// The API message one stream is delivering, keyed by parent tool use.
struct Stream {
    message: String,
    /// Set once the live blocks reach their bound. The message's remaining
    /// blocks then appear whole with their assistant frames, not streamed.
    truncated: bool,
}

struct Submission {
    preview: chat::QueuedMessage,
    mode: PromptMode,
    /// Queued input enters the transcript only when the provider consumes it.
    deferred: Option<Vec<Item>>,
    /// Encoded input bounds retained text and image data across the queue.
    deferred_bytes: usize,
}

#[derive(Clone, Copy)]
enum ReportTask {
    Agent,
    Workflow,
}

/// How a background task ended, as its notification reported it.
#[derive(Clone, Debug, PartialEq)]
struct TaskOutcome {
    status: ToolStatus,
    exit_code: Option<i32>,
    summary: String,
}

impl TaskOutcome {
    fn new(status: ToolStatus, summary: &str) -> Self {
        Self {
            status,
            exit_code: summary_exit_code(summary),
            summary: summary.trim().to_owned(),
        }
    }

    /// The outcome as a restored history keeps it for every background task
    /// it records: a summary long enough to say how the task ended, rather
    /// than one quoting a whole command.
    fn recorded(mut self) -> Self {
        if self.summary.len() > RECORDED_SUMMARY_BYTES {
            self.summary
                .truncate(self.summary.floor_char_boundary(RECORDED_SUMMARY_BYTES));
            self.summary.push('…');
            self.summary.shrink_to_fit();
        }
        self
    }

    /// Settle the call that started the task. A command shows how it exited;
    /// any other call, such as a Monitor, only how it ended.
    fn settle(&self, content: &mut ItemContent) {
        match content {
            ItemContent::Command {
                status,
                exit_code,
                output,
                action: CommandAction::Run,
                ..
            } => {
                *status = self.status;
                *exit_code = self.exit_code.or(*exit_code);
                if output.is_empty() {
                    output.clone_from(&self.summary);
                }
            }
            ItemContent::Command { status, .. } | ItemContent::Tool { status, .. } => {
                *status = self.status;
            }
            _ => {}
        }
    }
}

/// A content block of a message still streaming: the item it is, and for a
/// tool call, what its input says so far.
struct Block {
    item: Item,
    input: StreamedInput,
}

#[derive(Default)]
struct Claude {
    session_id: Option<String>,
    prepared_identity: Option<PreparedIdentity>,
    titles: Option<super::claude_titles::ClaudeTitles>,
    /// Whether this process was launched to resume `session_id`, as opposed
    /// to being handed a new one that names nothing until its first prompt.
    resumed: bool,
    cwd: PathBuf,
    ready: bool,
    busy: bool,
    /// Only advertised for a CLI whose async input contract we have verified.
    async_input: bool,
    submissions: HashMap<String, Submission>,
    queued: Vec<chat::QueuedMessage>,
    interruption: Option<bool>,
    attention_generation: u64,
    suggestion_turn: Option<(u64, String)>,
    /// Which result-delimited turn has already published its start. Busy may
    /// stay true between turns while background permission decisions remain.
    started_generation: Option<u64>,
    deferred_attention: HashMap<
        u64,
        (
            Option<String>,
            crate::notifications::Kind,
            Option<crate::model::RunId>,
        ),
    >,
    execution: Option<crate::model::RunId>,
    pending: HashMap<String, Value>,
    retired: VecDeque<String>,
    controls: HashMap<String, Value>,
    unknown_frames: HashSet<String>,
    streams: HashMap<String, Stream>,
    /// Blocks of the messages still streaming. Claude emits each block's
    /// assistant frame before that block stops, so a message's blocks are
    /// dropped at its `message_stop`, or when its stream starts another message.
    blocks: HashMap<(ScopedId, u64), Block>,
    /// Unknown block types still own their stream index. Their later deltas
    /// must not mutate an older block or look like an orphan known block.
    ignored_blocks: HashSet<(ScopedId, u64)>,
    stream_events: VecDeque<String>,
    carrier_offsets: HashMap<String, usize>,
    message_offsets: HashMap<ScopedId, usize>,
    message_order: VecDeque<ScopedId>,
    carrier_items: HashMap<String, Vec<String>>,
    carrier_parent: HashMap<String, String>,
    carrier_order: VecDeque<String>,
    retracted: std::collections::HashSet<String>,
    retracted_order: VecDeque<String>,
    tools: HashMap<ScopedId, Item>,
    /// Agent calls that published what their agent goes by. A child's own
    /// frames carry its type and task, which must not rename it. Bounded by
    /// `tools`.
    labelled_agents: HashSet<String>,
    /// Synthetic task rows shown before their Agent tool is known. Bounded by
    /// the same cache as task tools, so late correlation can retract one row.
    visible_task_rows: HashSet<String>,
    /// The tool each call shown as a file change was made with. A change has
    /// no place for the name, and an edit that fails is shown as its call
    /// again; the input says which tool that was only while it is still
    /// held, which a large write's never is.
    edit_tools: HashMap<ScopedId, String>,
    tool_inputs: HashMap<ScopedId, String>,
    tool_input_order: VecDeque<ScopedId>,
    dismissed_denials: VecDeque<String>,
    permissions: Option<super::claude_permissions::Snapshot>,
    tool_order: VecDeque<ScopedId>,
    /// Running background tasks, oldest first.
    tasks: VecDeque<String>,
    /// Bounded origin metadata, not running state. Preserve the explicit
    /// native kind and the name the task started with through progress and
    /// terminal-notification replay: a progress frame's description is what
    /// the agent is doing now, such as "Running cat > …", not its name.
    report_tasks: VecDeque<(String, ReportTask, String)>,
    /// Background tasks Claude Code reported a shell command started, and
    /// those a subagent started, oldest first. Only a task's start says
    /// either; its progress and notification frames do not repeat it.
    command_tasks: VecDeque<String>,
    subagent_tasks: VecDeque<String>,
    /// Commands still running in the foreground of the call that started
    /// them. Claude Code reports a long foreground command as a task too, and
    /// names it by the command when it has no description; its notification
    /// is that call's own progress, which its result settles, and never
    /// reaches the model. Moving it to the background makes it a background
    /// task like any other.
    foreground_tasks: VecDeque<String>,
    /// How each background task a call started ended, by that call, oldest
    /// first: its status, the exit status its summary gave, and the summary.
    /// A replayed launch acknowledgement is settled from it.
    task_outcomes: VecDeque<(String, TaskOutcome)>,
    /// How each background task ended, by the call that started it, as the
    /// restored transcript and its agents' sidecars recorded it. It is kept
    /// whole rather than bounded by count, since a replayed launch it does not
    /// name is shown as never having been settled.
    history_outcomes: HashMap<String, TaskOutcome>,
    /// Background commands still waiting for their task's notification, by
    /// the call that launched them, oldest first.
    launches: VecDeque<ScopedId>,
    /// The rows of those commands that the tool cache has since let go of.
    /// A long command can outlast hundreds of later calls; its row is still
    /// on screen as Running, and its notification must still settle it.
    evicted_launches: HashMap<ScopedId, Item>,
    /// Exact successful child failure/result and task-notification receipts,
    /// not bodies or running work. Changed payloads remain observable.
    failure_receipts: VecDeque<[u8; 32]>,
    /// Agents whose report was handed back as a peer message, newest last,
    /// so the notification that follows it does not show it again.
    handed_back: VecDeque<String>,
    background_agents: HashSet<String>,
    /// Agents whose settled run `SendMessage` started again: their tool row
    /// still shows the earlier outcome, which the next receipt must replace.
    restarted_agents: VecDeque<String>,
    /// The background agents last reported as waiting on background work of
    /// their own. Any sign of life from one means it has resumed.
    waiting_agents: HashSet<String>,
    /// The agent ID Claude Code gave each agent it runs in the background,
    /// with the Agent tool call that started it, oldest first. A task's frames
    /// name the agent by this ID, and after `SendMessage` resumes it, by no
    /// tool call of its own.
    agent_calls: VecDeque<(String, String)>,
    /// The Agent tool calls whose agent ID this conversation has reported,
    /// so a replayed page does not report again what its history did.
    addressed_agents: HashSet<String>,
    /// The Agent tool call each agent task Claude reported was started by,
    /// with the task, oldest first: what stopping one subagent addresses,
    /// whether it runs in the foreground or the background.
    agent_task_calls: VecDeque<(String, String)>,
    /// Agent tool calls whose agent ended, with how, oldest first, so a
    /// delayed launch acknowledgement cannot reopen it and a refused result
    /// cannot turn a stop into a failure.
    ended_agents: VecDeque<(String, ToolStatus)>,
    /// Earlier tool occurrences of an explicitly resumed provider agent.
    /// Receipts for them must not revive or fail the current occurrence.
    superseded_agents: VecDeque<String>,
    /// Calls that started an agent or a forked skill inside another agent.
    /// Claude Code forwards their conversations with `parent_tool_use_id`
    /// naming that call; they are a worker's helpers, never children of this
    /// conversation. Kept apart from the tool cache, which a busy team of
    /// workers turns over long before their helpers finish.
    nested_calls: NestedIds,
    /// Messages sent inside another agent, which can resume a helper under
    /// that call. Only its forwarded conversation is set aside by them: a
    /// worker can also message one of this conversation's own agents, whose
    /// lifecycle then names that call.
    nested_messages: NestedIds,
    /// The tasks of those helpers, and the tasks they start. Their lifecycle
    /// frames reach this conversation too, the later ones naming only the
    /// task.
    nested_tasks: NestedIds,
    /// Agents the background level ended before their notice arrived. That
    /// notice, when it comes, still says how they went.
    unreported_agents: VecDeque<String>,
    /// Every live background task, by task ID, as Claude Code's last
    /// `background_tasks_changed` level reported it in this process; `None`
    /// until the first one. A task that leaves it is no longer running,
    /// whether or not its own notification arrived.
    background_level: Option<HashSet<String>>,
    /// The call each background shell task Claude Code reported belongs to,
    /// oldest first, so the level can settle that call's row.
    command_task_calls: VecDeque<(String, String)>,
    /// Historical runs with no live process evidence yet. A fresh child
    /// carrier or API stream can prove that this occurrence resumed.
    unconfirmed_agents: HashSet<String>,
    /// Current failure notices, retired only by an accepted human prompt.
    recent_failed_agents: VecDeque<String>,
    /// Consumed human inputs; replay acknowledgements must not advance scope.
    accepted_user_prompts: VecDeque<String>,
    models: Vec<Model>,
    /// The full model ID each listed model resolves to, where Claude Code
    /// said: `opus` to `claude-opus-5-5`, `fable` to `claude-fable-5-1`.
    resolved: HashMap<String, String>,
    resolved_labels: HashMap<String, String>,
    /// The listed model the conversation is on: the one launched with,
    /// chosen since, or the listed model the latest turn's full ID stands for.
    selected: Option<String>,
    /// The full model ID the latest turn reported running on, which keys the
    /// result's per-model usage.
    running: Option<String>,
    running_reasoning: Option<String>,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning: Option<String>,
    fast_mode: Option<chat::FastMode>,
    commands: Vec<SlashCommand>,
    /// The commands the session's latest described list names without
    /// Claude Code's mark for its own: the user's, the project's and
    /// plugins'. Empty when the list marks none, as a version without the
    /// mark sends it, since then nothing tells them apart.
    authored: HashSet<String>,
    /// The skills the session reported when it began its latest turn.
    skills: HashSet<String>,
    /// The name each command goes by in a turn's lists, keyed to the name
    /// Claude Code shows for it. A plugin's skill is shown by its own name
    /// but listed by the plugin's, and the session gives the second as an
    /// alias of the first.
    aliases: HashMap<String, String>,
    usage: Usage,
    history: Option<History>,
    /// Why a resumed conversation's saved history could not be read. The
    /// provider still resumes it; the transcript says what is missing.
    history_error: Option<String>,
    /// A saved directory is gone; only its exact persisted history is readable.
    history_only: bool,
    forced_history: bool,
    /// A compaction boundary arrived, so the provider context that follows
    /// is the summary standing in for the turns before it.
    compaction_summary_due: bool,
    /// The running main turn received content this version cannot show.
    turn_unsupported_content: bool,
    /// The running main turn produced something the transcript can show.
    turn_visible_content: bool,
}
impl Claude {
    fn warn_unknown(&mut self, kind: &str, name: &str) {
        // Provider-controlled names cannot grow the rate-limit map forever.
        let key = format!("{kind}:{}", super::diagnostic_preview(name.as_bytes(), 128));
        if self.unknown_frames.len() < 64 && self.unknown_frames.insert(key.clone()) {
            tracing::warn!(frame_type = %key, "Skipping unknown Claude protocol frame");
        }
    }

    /// Replayed history comes from before this process started, so a step
    /// it leaves running ended with an earlier process, such as one the
    /// system stopped. A background agent stays running: another process on
    /// this conversation may still be running it. A read-only view shows the
    /// history as it is.
    fn stop_earlier_steps(&self, items: &mut [Item]) {
        if self.history_only {
            return;
        }
        for item in items {
            if !self.background_agents.contains(&item.id)
                && let Some(status) = item.content.status_mut()
                && *status == ToolStatus::Running
            {
                *status = ToolStatus::Stopped;
            }
        }
    }

    fn remember_failure_receipt(&mut self, receipt: Option<[u8; 32]>) {
        if let Some(receipt) = receipt {
            self.failure_receipts.push_back(receipt);
            if self.failure_receipts.len() > MAX_TOOLS {
                self.failure_receipts.pop_front();
            }
        }
    }

    fn observe_title(&mut self, step: &mut Step) {
        if let Some(id) = self.session_id.clone()
            && let Some(titles) = &mut self.titles
            && let Some(title) = titles.update(&id)
        {
            step.events.push(Event::Session {
                id,
                title: Some(title),
            });
        }
    }

    fn control(&mut self, mut request: Value) -> Result<Step, String> {
        if self.controls.len() >= MAX_PENDING {
            return Err("Too many pending Claude control operations".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.controls.insert(id.clone(), request.clone());
        let wire = request
            .as_object_mut()
            .ok_or("Malformed native Claude control operation")?;
        wire.remove("_cagents_reasoning");
        wire.remove("_cagents_turn");
        wire.remove("_cagents_permissions");
        wire.remove("_cagents_permission_update");
        Ok(Step {
            suppress_current_notification: false,
            notification_preview: None,
            writes: vec![json!({"type":"control_request","request_id":id,"request":request})],
            events: vec![],
        })
    }
    fn fast_mode_metadata(&mut self, data: &Value, step: &mut Step) {
        if let Some(state) = data["fast_mode_state"]
            .as_str()
            .and_then(|state| match state {
                "off" => Some(chat::FastModeState::Off),
                "on" => Some(chat::FastModeState::On),
                "cooldown" => Some(chat::FastModeState::Cooldown),
                _ => None,
            })
        {
            let value = chat::FastMode {
                state,
                disabled_reason: data["fast_mode_disabled_reason"]
                    .as_str()
                    .map(str::to_owned),
            };
            if self.fast_mode.as_ref() != Some(&value) {
                self.fast_mode = Some(value.clone());
                step.events.push(Event::FastMode(value));
                step.events.push(Event::Capabilities(self.capabilities()));
            }
        }
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            steer: self.async_input,
            queue: self.async_input,
            queue_remove: self.async_input,
            attachments: true,
            compact: self.commands.iter().any(|c| c.name == "compact"),
            model_selection: !self.models.is_empty(),
            reasoning_selection: self.models.iter().any(|m| !m.reasoning.is_empty()),
            fast_mode: self
                .fast_mode
                .as_ref()
                .is_some_and(chat::FastMode::changeable),
            permission_modes: MODES.iter().map(|s| (*s).into()).collect(),
            skill_reach: SkillReach::Leading,
            stop_subagent: true,
            // Headless Claude Code's interrupt aborts the controller its
            // background agents run under, so it stops them with the turn.
            interrupt_stops_background: true,
            ..Capabilities::default()
        }
    }
    fn accept_user_prompt(&mut self, id: &str, step: &mut Step) {
        if self.accepted_user_prompts.iter().any(|known| known == id) {
            return;
        }
        self.accepted_user_prompts.push_back(id.into());
        if self.accepted_user_prompts.len() > 2048 {
            self.accepted_user_prompts.pop_front();
        }
        for id in self.recent_failed_agents.drain(..) {
            self.unconfirmed_agents.insert(id.clone());
            step.events.push(subagent_status(
                id,
                ToolStatus::Ended,
                Some("Failed before the current prompt; see its transcript"),
                false,
            ));
        }
    }

    fn start_turn(&mut self, step: &mut Step) {
        if self.started_generation != Some(self.attention_generation) {
            self.suggestion_turn = None;
            step.events.push(Event::PromptSuggestionCleared);
            self.started_generation = Some(self.attention_generation);
            step.events.push(Event::TurnStarted);
        }
    }
    fn status(&mut self, busy: bool, detail: impl Into<String>, step: &mut Step) {
        self.busy = busy;
        if !busy {
            step.events.push(Event::Waiting(false));
        }
        step.events.push(Event::Status {
            busy,
            detail: detail.into(),
        });
    }
    fn retire(&mut self, id: String, step: &mut Step) {
        self.pending.remove(&id);
        step.events.push(Event::RequestResolved { id: id.clone() });
        // A background decision can arrive before or after the main result,
        // so the adapter may already be idle when it is answered. Once no
        // request or active turn remains, forget between-turn interruption:
        // this idle generation also identifies the next prompt.
        if self.pending.is_empty() && self.started_generation != Some(self.attention_generation) {
            self.interruption = None;
            // This idle generation will also identify the next prompt. Forget
            // its between-turn interrupts before a late reply can be mistaken
            // for that prompt's own interrupt or deferred result.
            self.controls.retain(|_, request| {
                string(request, "subtype") != "interrupt"
                    || request["_cagents_turn"].as_u64() != Some(self.attention_generation)
            });
            self.status(false, "Claude turn complete", step);
        }
        self.retired.push_back(id);
        if self.retired.len() > 512 {
            self.retired.pop_front();
        }
    }
    fn tool(&mut self, parent: Option<&str>, item: Item) {
        let key = ScopedId::new(parent, &item.id);
        if !self.tools.contains_key(&key) {
            self.tool_order.push_back(key.clone());
        }
        self.tools.insert(key, item);
        while self.tool_order.len() > MAX_TOOLS {
            if let Some(key) = self.tool_order.pop_front() {
                if self.launches.contains(&key)
                    && let Some(item) = self.tools.get(&key)
                {
                    self.evicted_launches.insert(key.clone(), item.clone());
                }
                self.forget_tool(&key);
            }
        }
    }

    /// Remember a background command that is running until its task's
    /// notification arrives.
    fn launched(&mut self, key: ScopedId) {
        self.launches.retain(|known| known != &key);
        self.launches.push_back(key);
        if self.launches.len() > MAX_TOOLS
            && let Some(oldest) = self.launches.pop_front()
        {
            self.evicted_launches.remove(&oldest);
        }
    }
    fn forget_tool(&mut self, key: &ScopedId) {
        self.tools.remove(key);
        self.labelled_agents.remove(&key.id);
        self.edit_tools.remove(key);
        self.tool_inputs.remove(key);
        self.tool_input_order.retain(|known| known != key);
        if key.parent.is_none() {
            self.visible_task_rows.remove(&key.id);
        }
    }
    fn completed_tool(&self, parent: Option<&str>, id: &str) -> bool {
        self.tools
            .get(&ScopedId::new(parent, id))
            .is_some_and(|old| {
                old.content
                    .status()
                    .is_some_and(|status| status != ToolStatus::Running)
            })
    }
    fn task_rows_for_agent(&mut self, parent: Option<&str>, tool: &str) -> Vec<String> {
        let incoming = ScopedId::new(parent, tool);
        if self
            .tools
            .keys()
            .any(|known| known.id == tool && known.parent != incoming.parent)
        {
            // Task notifications carry no transcript. Keep their fallback row
            // while more than one scope could own the reported tool ID.
            return Vec::new();
        }
        self.agent_task_calls
            .iter()
            .filter(|(known, _)| known == tool)
            .map(|(_, task)| format!("claude-task:{task}"))
            .filter(|id| self.visible_task_rows.remove(id))
            .collect()
    }
    fn remember_tool_input(
        &mut self,
        parent: Option<&str>,
        id: &str,
        input: &Value,
    ) -> Result<(), String> {
        if !input.is_object() || encoded_len(input)? > chat::PermissionDenial::INPUT_LIMIT {
            return Ok(());
        }
        let key = ScopedId::new(parent, id);
        if !self.tool_inputs.contains_key(&key) {
            self.tool_input_order.push_back(key.clone());
        }
        self.tool_inputs.insert(key, input.to_string());
        while self.tool_input_order.len() > chat::PermissionDenial::LIMIT {
            if let Some(expired) = self.tool_input_order.pop_front() {
                self.tool_inputs.remove(&expired);
            }
        }
        Ok(())
    }
    fn send_prompt(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
        mode: PromptMode,
    ) -> Result<Step, String> {
        if !self.ready {
            return Err("Claude is still initializing".into());
        }
        if self.busy && mode == PromptMode::Send {
            return Err("Claude is busy; interrupt or wait before sending another prompt".into());
        }
        if mode != PromptMode::Send && !self.async_input {
            return Err(
                "This Claude version does not advertise verified async message delivery".into(),
            );
        }
        if self.submissions.len() >= chat::QueuedMessage::LIMIT {
            return Err("Too many pending Claude messages".into());
        }
        if text.len() > MAX_TEXT || attachments.len() > ATTACHMENTS.max_count.unwrap() {
            return Err("Claude prompt exceeds the native transport limit".into());
        }
        if text.trim().is_empty() && attachments.is_empty() {
            return Err("A Claude prompt cannot be empty".into());
        }
        if matches!(
            text.split_whitespace().next(),
            Some("/resume" | "/fork")
        ) {
            return Err(
                "Claude resume/fork commands require opening a separate native session".into(),
            );
        }
        let mut content = vec![json!({"type":"text", "text":text})];
        let mut payload_size = encoded_len(&content)?;
        for attachment in super::image_references::ordered(&text, &attachments)? {
            let attachment = crate::chat::attachment_assets::resolve(attachment)?;
            let block = attachment_block(&self.cwd, &attachment.path)?;
            payload_size = payload_size.saturating_add(encoded_len(&block)? + 1);
            if payload_size >= MAX_PAYLOAD {
                return Err(
                    "Combined Claude prompt and attachments must be smaller than 30 MiB".into(),
                );
            }
            if let Some(reference) = &attachment.reference {
                if block["type"] != "image" {
                    return Err("An image reference must accompany an actual image".into());
                }
                // Claude Code tells the model its own copy's path.
                let marker =
                    json!({"type":"text", "text":super::image_references::caption(reference)?});
                payload_size = payload_size.saturating_add(encoded_len(&marker)? + 1);
                if payload_size >= MAX_PAYLOAD {
                    return Err(
                        "Combined Claude prompt and attachments must be smaller than 30 MiB".into(),
                    );
                }
                content.push(marker);
            }
            content.push(block);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut frame = json!({"type":"user", "uuid":id, "parent_tool_use_id":null, "origin":{"kind":"human"}, "message":{"role":"user", "content":content}});
        if let Some(session) = &self.session_id {
            frame["session_id"] = json!(session);
        }
        if encoded_len(&frame)? >= MAX_PAYLOAD {
            return Err("Serialized Claude prompt must be smaller than 30 MiB".into());
        }
        let mut step = Step::default();
        if self.async_input {
            frame["priority"] = match mode {
                PromptMode::Send | PromptMode::Steer => "next",
                PromptMode::Queue => "later",
            }
            .into();
        }
        let deferred_bytes = if mode != PromptMode::Send {
            let bytes = encoded_len(&frame)?;
            let retained: usize = self
                .submissions
                .values()
                .map(|submission| submission.deferred_bytes)
                .sum();
            if bytes > MAX_PAYLOAD.saturating_sub(retained) {
                return Err("Claude queued messages exceed the 30 MiB retained input limit; wait for delivery or remove a queued message".into());
            }
            bytes
        } else {
            0
        };
        for attachment in &attachments {
            crate::chat::attachment_assets::retain(attachment)?;
        }
        self.suggestion_turn = None;
        step.events.push(Event::PromptSuggestionCleared);
        self.message(&frame, &mut step)?;
        if self.async_input {
            let mut preview = chat::QueuedMessage::new(id.clone(), text);
            preview.attachments = attachments;
            if mode == PromptMode::Steer {
                preview.delivery = chat::QueueDelivery::NextStep;
            }
            let deferred = if mode != PromptMode::Send {
                let mut items = Vec::new();
                for event in std::mem::take(&mut step.events) {
                    if let Event::Item(item) = event {
                        items.push(item);
                    } else {
                        step.events.push(event);
                    }
                }
                // "next" input precedes "later" input; preserve admission
                // order within each priority instead of reversing steers.
                let position = if mode == PromptMode::Steer {
                    self.queued
                        .iter()
                        .position(|queued| {
                            self.submissions
                                .get(&queued.id)
                                .is_some_and(|submission| submission.mode == PromptMode::Queue)
                        })
                        .unwrap_or(self.queued.len())
                } else {
                    self.queued.len()
                };
                self.queued.insert(position, preview.clone());
                step.events.push(Event::QueueChanged(self.queued.clone()));
                Some(items)
            } else {
                None
            };
            self.submissions.insert(
                id.clone(),
                Submission {
                    preview,
                    mode,
                    deferred,
                    deferred_bytes,
                },
            );
        }
        step.writes.push(frame);
        if !self.busy && mode == PromptMode::Send {
            self.start_turn(&mut step);
            self.status(true, "Claude is working", &mut step);
        }
        Ok(step)
    }
    fn answer(&mut self, id: String, action: String, answers: Vec<Answer>) -> Result<Step, String> {
        if answers.len() > 32
            || answers.iter().any(|a| a.values.len() > 64)
            || answers
                .iter()
                .flat_map(|a| &a.values)
                .map(String::len)
                .sum::<usize>()
                > MAX_TEXT
        {
            return Err("Claude form response exceeds its bounded transport limit".into());
        }
        let raw = self
            .pending
            .get(&id)
            .ok_or("Claude request is stale, cancelled, or already answered")?;
        let request = &raw["request"];
        if string(request, "subtype") == "elicitation" {
            return self.answer_elicitation(id, action, answers);
        }
        if string(request, "subtype") != "can_use_tool"
            || (request["requires_user_interaction"].as_bool() == Some(true)
                && string(request, "tool_name") != "AskUserQuestion")
        {
            return Err("This Claude request requires an unsupported local consent dialog; it cannot be generically approved or denied".into());
        }
        let response = match action.as_str() {
            "deny" => {
                json!({"behavior":"deny", "message": if string(request, "tool_name") == "AskUserQuestion" {
                    "The user skipped these questions"
                } else {
                    "The user denied this request"
                }})
            }
            "allow" | "allow_session" => {
                let mut input = request["input"].clone();
                if string(request, "tool_name") == "AskUserQuestion" {
                    let fields = question_fields(&input)?;
                    if answers.len() != fields.len() {
                        return Err("Answer every Claude question before submitting".into());
                    }
                    let mut result = serde_json::Map::new();
                    for field in fields {
                        let matching: Vec<_> =
                            answers.iter().filter(|a| a.field == field.id).collect();
                        if matching.len() != 1 {
                            return Err(format!("Missing or duplicate answer for {}", field.label));
                        }
                        let values = &matching[0].values;
                        if values.is_empty()
                            || (!field.multiple && values.len() != 1)
                            || values.iter().any(|s| s.trim().is_empty())
                        {
                            return Err(format!("Invalid answer for {}", field.label));
                        }
                        result.insert(field.id, Value::String(values.join(", ")));
                    }
                    input
                        .as_object_mut()
                        .ok_or("Claude question input is not an object")?
                        .insert("answers".into(), Value::Object(result));
                } else if !answers.is_empty() {
                    return Err("This Claude approval does not accept form answers".into());
                }
                let mut response = json!({"behavior":"allow", "updatedInput":input});
                if action == "allow_session" {
                    response["updatedPermissions"] = session_permission_suggestions(request)
                        .ok_or("This request has no safe scoped session permission suggestion")?;
                }
                response
            }
            _ => return Err("Unsupported Claude decision; explicitly allow or deny".into()),
        };
        if encoded_len(&response)? >= MAX_PAYLOAD {
            return Err("Claude permission response must be smaller than 30 MiB".into());
        }
        let mut step = Step {
            suppress_current_notification: false,
            notification_preview: None,
            writes: vec![
                json!({"type":"control_response", "response":{"subtype":"success", "request_id":id, "response":response}}),
            ],
            events: vec![],
        };
        if action == "deny"
            && string(request, "tool_name") != "AskUserQuestion"
            && let Some(tool_use_id) = request["tool_use_id"].as_str()
        {
            self.permission_denied(
                None,
                &json!({
                    "tool_use_id": tool_use_id,
                    "tool_name": request["tool_name"],
                    "tool_input": request["input"],
                    "decision_reason_type": "userDenied",
                    "decision_reason": "You denied this request",
                    "agent_id": request["agent_id"],
                }),
                &mut step,
            )?;
        }
        self.retire(id, &mut step);
        Ok(step)
    }
    fn permission(&mut self, raw: Value) -> Result<Step, String> {
        let id = required(&raw, "request_id")?.to_owned();
        if self.retired.contains(&id) || self.pending.contains_key(&id) {
            return Ok(Step::default());
        }
        if self.pending.len() >= MAX_PENDING {
            return Err("Too many pending Claude permission requests".into());
        }
        let request = &raw["request"];
        if string(request, "subtype") == "elicitation" {
            return self.elicitation(raw);
        }
        if string(request, "subtype") == "request_user_dialog"
            || (request["requires_user_interaction"].as_bool() == Some(true)
                && string(request, "tool_name") != "AskUserQuestion")
        {
            let message = "Claude requested a local-only or unsupported consent dialog. It remains pending; interrupt to cancel this turn.";
            let prompt = Request {
                kind: chat::RequestKind::Consent,
                id: id.clone(),
                title: "Unsupported Claude consent dialog".into(),
                description: format!("{message}\n{request}"),
                fields: vec![],
                actions: vec![],
            };
            self.pending.insert(id, raw);
            return Ok(Step {
                events: vec![
                    Event::Request(prompt),
                    Event::Error {
                        message: message.into(),
                        fatal: false,
                    },
                ],
                ..Step::default()
            });
        }
        if string(request, "subtype") != "can_use_tool" {
            let error = format!(
                "Unsupported Claude control request: {}",
                string(request, "subtype")
            );
            return Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: vec![
                    json!({"type":"control_response", "response":{"subtype":"error", "request_id":id, "error":error}}),
                ],
                events: vec![Event::Error {
                    message: error,
                    fatal: false,
                }],
            });
        }
        let name = required(request, "tool_name")?;
        if !request["input"].is_object() {
            return Err("Claude permission request has malformed input".into());
        }
        let question = name == "AskUserQuestion";
        let fields = if question {
            question_fields(&request["input"])?
        } else {
            vec![]
        };
        let description = if question {
            "Claude is waiting for your answers.".to_owned()
        } else {
            permission_description(
                name,
                &request["input"],
                &[
                    string(request, "description"),
                    &clean_permission_text(string(request, "decision_reason")),
                ],
            )
        };
        // Claude answers questions through can_use_tool too, but the reader
        // is submitting answers, not granting permission to run a tool.
        let mut actions = vec![choice(
            "allow",
            if question {
                "Submit answers"
            } else {
                "Allow once"
            },
        )];
        if !question && let Some(suggestions) = session_permission_suggestions(request) {
            let mut session = choice("allow_session", "Allow for session");
            session.description = format!("Session only: {suggestions}");
            actions.push(session);
        }
        actions.push(choice(
            "deny",
            if question { "Skip question" } else { "Deny" },
        ));
        let event = Request {
            kind: if question {
                chat::RequestKind::Question
            } else {
                chat::RequestKind::Approval
            },
            id: id.clone(),
            title: if question {
                "Claude questions".to_owned()
            } else {
                format!("Claude: {name}")
            },
            description,
            fields,
            actions,
        };
        self.pending.insert(id, raw);
        Ok(Step {
            events: vec![Event::Request(event)],
            ..Step::default()
        })
    }
    /// The listed model a turn running on `model` is on. A turn names the
    /// full ID it runs on, `claude-opus-5-5`, while the list offers aliases
    /// that resolve to it, such as `default` and `opus`. The model already
    /// chosen keeps its place when it resolves there, so choosing Opus goes
    /// on showing Opus; otherwise the first listed model that resolves there
    /// stands for it. A model the list does not name is shown as it is.
    fn listed<'a>(&'a self, model: &'a str) -> &'a str {
        let resolves = |id: &str| self.resolved.get(id).is_some_and(|full| full == model);
        if self.models.iter().any(|listed| listed.id == model) {
            return model;
        }
        if let Some(selected) = self.selected.as_deref().filter(|id| resolves(id)) {
            return selected;
        }
        self.models
            .iter()
            .map(|listed| listed.id.as_str())
            .find(|id| resolves(id))
            .unwrap_or(model)
    }

    fn metadata(&mut self, data: &Value, step: &mut Step) {
        if let Some(version) = data["claude_code_version"].as_str() {
            self.async_input = supports_async_input(version);
        }
        if let Some(models) = data["models"].as_array() {
            self.models = models.iter().filter_map(Model::from_claude).collect();
            Model::resolve_claude_labels(&mut self.models, models);
            self.resolved_labels = models
                .iter()
                .filter_map(|model| {
                    let id = model["resolvedModel"].as_str()?;
                    Some((id.to_owned(), Model::claude_model_label(models, id)))
                })
                .collect();
            self.resolved = models
                .iter()
                .filter_map(|model| {
                    Some((
                        model["value"].as_str()?.to_owned(),
                        model["resolvedModel"].as_str()?.to_owned(),
                    ))
                })
                .collect();
        }
        if let Some(commands) = data["commands"].as_array() {
            let mut authored = HashSet::new();
            let mut marked = false;
            self.aliases.clear();
            self.commands = commands
                .iter()
                .filter_map(|c| {
                    let name: String = c["name"].as_str()?.trim_start_matches('/').into();
                    marked |= c.get("builtin").is_some();
                    for alias in c["aliases"].as_array().into_iter().flatten() {
                        if let Some(alias) = alias.as_str() {
                            self.aliases
                                .insert(alias.trim_start_matches('/').into(), name.clone());
                        }
                    }
                    if c["builtin"].as_bool() != Some(true) {
                        authored.insert(name.clone());
                    }
                    Some(SlashCommand {
                        name,
                        description: string(c, "description").into(),
                    })
                })
                .collect();
            if !marked {
                authored.clear();
            }
            self.authored = authored;
        } else if let Some(commands) = data["slash_commands"].as_array() {
            // The turn's own list names the commands without describing
            // them, so a description the session already gave is kept. A
            // command listed both by its name and by an alias of it is one.
            let described = std::mem::take(&mut self.commands);
            for listed in commands.iter().filter_map(Value::as_str) {
                let name = self.shown(listed);
                if self.commands.iter().any(|known| known.name == name) {
                    continue;
                }
                let description = described
                    .iter()
                    .find(|known| known.name == name)
                    .map(|known| known.description.clone())
                    .unwrap_or_default();
                self.commands.push(SlashCommand {
                    name: name.into(),
                    description,
                });
            }
        }
        if let Some(skills) = data["skills"].as_array() {
            self.skills = skills
                .iter()
                .filter_map(Value::as_str)
                .map(|name| self.shown(name).to_owned())
                .collect();
        }
        let terminal = data["terminal_slash_commands"].as_array();
        self.commands.retain(|c| {
            !matches!(c.name.as_str(), "resume" | "fork")
                && !terminal.is_some_and(|commands| {
                    commands.iter().any(|v| v.as_str() == Some(c.name.as_str()))
                })
        });
        if let Some(model) = data["model"].as_str() {
            self.selected = Some(self.listed(model).to_owned());
            self.running = Some(
                self.resolved
                    .get(model)
                    .map_or(model, String::as_str)
                    .to_owned(),
            );
            self.running_reasoning = self.reasoning.clone();
        }
        let active = self
            .running
            .as_ref()
            .filter(|id| {
                !matches!(
                    id.as_str(),
                    "default" | "opus" | "sonnet" | "haiku" | "opusplan"
                )
            })
            .map(|id| {
                let label = self
                    .resolved_labels
                    .get(id)
                    .cloned()
                    .or_else(|| {
                        self.models
                            .iter()
                            .find(|model| model.id == *id)
                            .map(|model| model.label.clone())
                    })
                    .unwrap_or_else(|| id.clone());
                let mut aliases: Vec<_> = self
                    .resolved
                    .iter()
                    .filter(|&(_alias, resolved)| resolved == id).map(|(alias, _resolved)| alias.clone())
                    .collect();
                aliases.sort_unstable();
                aliases.dedup();
                chat::ActiveModel {
                    id: id.clone(),
                    label,
                    reasoning: self.running_reasoning.clone(),
                    default_reasoning: None,
                    aliases,
                }
            });
        step.events.push(Event::ActiveModel(active));
        step.events.extend([
            Event::Models {
                models: self.models.clone(),
                selected: self.selected.clone(),
                reasoning: self.reasoning.clone(),
            },
            Event::Commands(
                self.commands
                    .iter()
                    .filter(|c| !self.is_skill(&c.name))
                    .cloned()
                    .collect(),
            ),
            Event::Skills(
                self.commands
                    .iter()
                    .filter(|c| self.is_skill(&c.name))
                    .map(|c| Skill {
                        name: c.name.clone(),
                        description: c.description.clone(),
                        invocation: format!("/{}", c.name),
                    })
                    .collect(),
            ),
            Event::Capabilities(self.capabilities()),
        ]);
    }
    /// The name Claude Code shows for a command a turn's list names.
    fn shown<'a>(&'a self, listed: &'a str) -> &'a str {
        let listed = listed.trim_start_matches('/');
        self.aliases.get(listed).map_or(listed, String::as_str)
    }
    /// Whether a listed command is a skill: a prompt the model can be pointed
    /// at from anywhere in a message, which Claude Code runs through its Skill
    /// tool, rather than a command it only runs as the whole of one.
    ///
    /// Claude Code lists its skills among its slash commands and names them
    /// apart only when a turn begins, and that list leaves out some the model
    /// can still be pointed at: a command in `.claude/commands`, and a skill
    /// without a description. So a command is also a skill when the latest
    /// described list does not mark it as Claude Code's own, as the user's,
    /// the project's and plugins' are, including one added mid-session. A
    /// command neither list vouches for stays a command, which is only ever
    /// offered where Claude Code acts on it, and so does every command of a
    /// list without the mark, which cannot tell `/clear` from a skill.
    ///
    /// An MCP server's prompt is shown as `server:prompt (MCP)` until a turn
    /// lists it as `mcp__server__prompt`; neither is ever a skill. A server
    /// reached over HTTP may show a prompt by its bare name, which nothing
    /// tells apart from a skill.
    ///
    /// A skill its author keeps from the model (`disable-model-invocation`)
    /// is still listed as one, and Claude Code refuses it when the model
    /// asks for it mid-message; nothing in the session tells it apart.
    fn is_skill(&self, name: &str) -> bool {
        if name.starts_with("mcp__") || name.ends_with(" (MCP)") {
            return false;
        }
        self.skills.contains(name) || self.authored.contains(name)
    }
    fn message(&mut self, frame: &Value, step: &mut Step) -> Result<(), String> {
        self.message_with_tools(frame, step, &HashMap::new(), false)
    }
    fn history_message(&mut self, record: &HistoryRecord, step: &mut Step) -> Result<(), String> {
        if let Some(gap) = record.gap {
            step.events.push(Event::Item(gap_item(gap)));
            return Ok(());
        }
        self.message_with_tools(&record.frame, step, &record.tool_calls, true)
    }
    fn message_with_tools(
        &mut self,
        frame: &Value,
        step: &mut Step,
        history_calls: &HistoryCalls,
        history: bool,
    ) -> Result<(), String> {
        let carrier = required(frame, "uuid")?;
        if self.retracted.contains(carrier) {
            return Ok(());
        }
        // Capture provenance before this carrier updates correlation state.
        // Replaying an old answer is not evidence that a blocked child resumed.
        let fresh_live_carrier = !history && !self.carrier_items.contains_key(carrier);
        self.retract(&frame["supersedes"], step)?;
        let child = frame["parent_tool_use_id"]
            .as_str()
            .filter(|id| !id.is_empty());
        let event_start = step.events.len();
        let mut global_task_removals = Vec::new();
        let role = if string(frame, "type") == "user" {
            Role::User
        } else {
            Role::Assistant
        };
        if let Some(parent) = child
            && fresh_live_carrier
            && role == Role::Assistant
            && frame["error"].is_null()
            && frame["isMeta"].as_bool() != Some(true)
            && (frame["message"]["content"]
                .as_str()
                .is_some_and(|text| !text.trim().is_empty())
                || frame["message"]["content"]
                    .as_array()
                    .is_some_and(|blocks| {
                        blocks.iter().any(|block| match string(block, "type") {
                            "text" => !string(block, "text").trim().is_empty(),
                            "thinking" => !string(block, "thinking").trim().is_empty(),
                            "redacted_thinking" => !string(block, "data").trim().is_empty(),
                            "tool_use" | "server_tool_use" => {
                                !string(block, "id").trim().is_empty()
                                    && !string(block, "name").trim().is_empty()
                            }
                            "tool_result" => !string(block, "tool_use_id").trim().is_empty(),
                            "image" => match string(&block["source"], "type") {
                                "url" => !string(&block["source"], "url").trim().is_empty(),
                                "base64" => {
                                    matches!(
                                        string(&block["source"], "media_type"),
                                        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                                    ) && !string(&block["source"], "data").trim().is_empty()
                                }
                                _ => false,
                            },
                            "document" => ["data", "url", "text"]
                                .iter()
                                .any(|field| !string(&block["source"], field).trim().is_empty()),
                            _ => false,
                        })
                    }))
        {
            step.events.push(Event::Subagent {
                id: parent.into(),
                kind: String::new(),
                description: String::new(),
                status: self
                    .unconfirmed_agents
                    .remove(parent)
                    .then_some(ToolStatus::Running),
                background: None,
                model: None,
                waiting: Some(false),
                detail: Some(String::new()),
            });
        }
        // Another agent's delivery is shown as one, including the hand-backs
        // Claude marks as meta.
        let delivered = if role == Role::User && !model_only(frame) {
            self.deliveries(frame)
                .into_iter()
                .map(|content| self.deduplicate(content))
                .collect()
        } else {
            Vec::new()
        };
        if fresh_live_carrier && role == Role::User && child.is_none() && !model_only(frame) {
            self.settle_live_notifications(frame, step);
        }
        // Explicit meta context is model-only. The wire isSynthetic marker is
        // broader: it also covers intentionally visible compact summaries.
        if delivered.is_empty() && frame["isMeta"].as_bool() == Some(true) {
            self.track_carrier(carrier, Vec::new(), child);
            return Ok(());
        }
        // Raw transcripts retain the more specific summary/transcript-only
        // flags that stream-json folds into isSynthetic. All preserve their
        // text under the same honest, generic provider-context disclosure.
        let context = role == Role::User
            && [
                "isSynthetic",
                "isCompactSummary",
                "isVisibleInTranscriptOnly",
            ]
            .iter()
            .any(|key| frame[*key].as_bool() == Some(true));
        // The transcript names its compaction summary; the wire folds that
        // flag away, so there the summary is the context that follows a
        // compaction boundary.
        let compaction = context
            && (frame["isCompactSummary"].as_bool() == Some(true)
                || (!history && child.is_none() && self.compaction_summary_due));
        if !history && child.is_none() {
            self.compaction_summary_due = false;
        }
        let context_item = if compaction {
            compaction_item
        } else {
            context_item
        };
        let content_role = if context { Role::System } else { role };
        let message = &frame["message"];
        let id = message["id"]
            .as_str()
            .or_else(|| frame["uuid"].as_str())
            .ok_or("Claude message is missing its stable identifier")?;
        if !delivered.is_empty() {
            for (index, content) in delivered.into_iter().enumerate() {
                step.events.push(Event::Item(Item {
                    id: format!("{id}:{index}"),
                    role,
                    content,
                    started_at: None,
                    at: None,
                }));
            }
        } else if let Some(content) = message["content"].as_str() {
            step.events.push(Event::Item(if context {
                context_item(format!("{id}:0"), content.into())
            } else {
                text_item(
                    format!("{id}:0"),
                    role,
                    if role == Role::User {
                        slash_command_prompt(content).unwrap_or_else(|| content.into())
                    } else {
                        content.into()
                    },
                    false,
                )
            }));
        } else if let Some(content) = message["content"].as_array() {
            let message_key = ScopedId::new(child, id);
            let base = frame["_cagents_block_base"]
                .as_u64()
                .map(|v| v as usize)
                .or_else(|| self.carrier_offsets.get(carrier).copied())
                .unwrap_or_else(|| self.message_offsets.get(&message_key).copied().unwrap_or(0));
            // Carrier offsets age out with `carrier_order`. A message's carriers
            // arrive together, so evicting the message first seen longest ago
            // leaves any message still arriving with its offset.
            if !self.carrier_offsets.contains_key(carrier) {
                self.carrier_offsets.insert(carrier.into(), base);
                if self
                    .message_offsets
                    .insert(message_key.clone(), base + content.len())
                    .is_none()
                {
                    self.message_order.push_back(message_key.clone());
                }
                while self.message_order.len() > 2048 {
                    if let Some(old) = self.message_order.pop_front() {
                        self.message_offsets.remove(&old);
                    }
                }
            }
            let mut synthetic = std::collections::HashSet::new();
            let mut references = std::collections::HashMap::new();
            let mut unverified = std::collections::HashMap::new();
            if role == Role::User {
                for (index, pair) in content.windows(2).enumerate() {
                    let marker = string(&pair[0], "text");
                    if string(&pair[0], "type") == "text"
                        && string(&pair[1], "type") == "image"
                        && let ItemContent::Image { path, .. } = image_content(&pair[1])
                        && super::image_references::claims(marker, &path)
                    {
                        if let Some(reference) = super::image_references::restore(marker, &path) {
                            synthetic.insert(index);
                            references.insert(index + 1, reference);
                        } else if !path.starts_with("data:image/") {
                            // A structured owned-image claim cannot authorize
                            // a coincident local path after receipt failure.
                            // Self-contained actual inline pixels remain safe.
                            unverified.insert(index + 1, format!("{} is unavailable on this device: its owned image receipt could not be verified. Use the originating device or attach the image again.", super::image_references::claimed_label(marker, &path)));
                        }
                    }
                }
            }
            for (index, block) in content.iter().enumerate() {
                if synthetic.contains(&index) {
                    continue;
                }
                let reference = references.get(&index).cloned();
                let unavailable = unverified.get(&index);
                let correlated_call = history_calls.get(&index);
                let index = self
                    .blocks
                    .iter()
                    .filter(|((message, block_index), Block { item, .. })| {
                        message == &message_key
                            && *block_index >= base as u64
                            && match (&item.content, string(block, "type")) {
                                (
                                    ItemContent::Text {
                                        text,
                                        thinking: false,
                                        ..
                                    },
                                    "text",
                                ) => text == string(block, "text"),
                                (
                                    ItemContent::Text {
                                        text,
                                        thinking: true,
                                        ..
                                    },
                                    "thinking",
                                ) => text == string(block, "thinking"),
                                (
                                    ItemContent::Command { .. }
                                    | ItemContent::Changes { .. }
                                    | ItemContent::WebSearch { .. }
                                    | ItemContent::Tool { .. },
                                    "tool_use" | "server_tool_use",
                                ) => item.id == string(block, "id"),
                                _ => false,
                            }
                    })
                    .map(|((_, index), _)| *index as usize)
                    .min()
                    .unwrap_or(base + index);
                let block_type = match required(block, "type") {
                    Ok(block_type) => block_type,
                    // One damaged saved block must not make the whole
                    // conversation unreadable. Live frames stay strict.
                    Err(_) if history => {
                        self.warn_unknown("content", "<untyped>");
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                match block_type {
                    "text" | "thinking" | "redacted_thinking" => {
                        let thinking = string(block, "type") != "text";
                        let text = if string(block, "type") == "redacted_thinking" {
                            "[Redacted thinking]"
                        } else {
                            string(block, if thinking { "thinking" } else { "text" })
                        };
                        step.events.push(Event::Item(if context {
                            context_item(format!("{id}:{index}"), text.into())
                        } else {
                            text_item(format!("{id}:{index}"), role, text.into(), thinking)
                        }));
                    }
                    "tool_use" | "server_tool_use" => {
                        let tool_id = required(block, "id")?.to_owned();
                        let item = Item {
                            id: tool_id.clone(),
                            role: Role::Assistant,
                            content: tool_content(
                                required(block, "name")?,
                                &block["input"],
                                &self.cwd,
                            ),
                            started_at: None,
                            at: None,
                        };
                        // A replayed background command whose acknowledgement
                        // the history lacks still ended as its recorded
                        // notification says; the acknowledgement, when it
                        // follows, settles it the same way.
                        let mut item = item;
                        if history
                            && block["input"]["run_in_background"].as_bool() == Some(true)
                            && matches!(item.content, ItemContent::Command { .. })
                            && let Some(outcome) = self.history_outcomes.get(&tool_id)
                        {
                            outcome.settle(&mut item.content);
                        }
                        // Replayed assistant envelopes must not reset a completed tool result.
                        if !self.completed_tool(child, &item.id) {
                            self.remember_tool_input(child, &tool_id, &block["input"])?;
                            if string(block, "name") == "Agent" {
                                global_task_removals
                                    .extend(self.task_rows_for_agent(child, &tool_id));
                                let background = child.is_none().then(|| {
                                    block["input"]["run_in_background"].as_bool() == Some(true)
                                });
                                if background == Some(true) {
                                    self.background_agents.insert(tool_id.clone());
                                }
                                let input = &block["input"];
                                let kind = agent_label(
                                    string(input, "name"),
                                    string(input, "description"),
                                    string(input, "subagent_type"),
                                );
                                if !kind.is_empty() {
                                    self.labelled_agents.insert(tool_id.clone());
                                }
                                step.events.push(Event::Subagent {
                                    model: block["input"]["model"].as_str().map(str::to_owned),
                                    waiting: None,
                                    detail: None,
                                    id: tool_id.clone(),
                                    kind: kind.into(),
                                    description: string(&block["input"], "description").into(),
                                    status: Some(ToolStatus::Running),
                                    background,
                                });
                            }
                            if matches!(item.content, ItemContent::Changes { .. }) {
                                self.edit_tools.insert(
                                    ScopedId::new(child, &tool_id),
                                    string(block, "name").into(),
                                );
                            }
                            self.tool(child, item.clone());
                            step.events.push(Event::Item(item));
                        } else if history {
                            // Keep the original call position as an overlap
                            // anchor without changing its confirmed outcome.
                            if let Some(completed) = self.tools.get(&ScopedId::new(child, &tool_id))
                            {
                                step.events.push(Event::Item(completed.clone()));
                            }
                        }
                    }
                    "tool_result" => {
                        let tool_id = required(block, "tool_use_id")?.to_owned();
                        let needs_call=self.tools.get(&ScopedId::new(child, &tool_id)).is_none_or(|old|matches!(&old.content,ItemContent::Tool{name,input,..} if name=="Tool"&&input.is_empty()));
                        let recovered = if needs_call {
                            if let Some(call) = correlated_call {
                                self.remember_tool_input(child, &tool_id, &call["input"])?;
                                let name = required(call, "name")?;
                                let content = tool_content(name, &call["input"], &self.cwd);
                                if matches!(&content, ItemContent::Changes { .. }) {
                                    self.edit_tools
                                        .insert(ScopedId::new(child, &tool_id), name.into());
                                }
                                Some(content)
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        let mut item = recovered
                            .map(|content| Item {
                                id: tool_id.clone(),
                                role: Role::Assistant,
                                content,
                                started_at: None,
                                at: None,
                            })
                            .or_else(|| self.tools.get(&ScopedId::new(child, &tool_id)).cloned())
                            .unwrap_or(Item {
                                id: tool_id.clone(),
                                role: Role::Assistant,
                                content: ItemContent::Tool {
                                    name: "Tool".into(),
                                    input: String::new(),
                                    output: String::new(),
                                    status: ToolStatus::Running,
                                },
                                started_at: None,
                                at: None,
                            });
                        // The structured result is the frame's, so it is
                        // this block's only when the frame has no other.
                        let structured = if content.len() == 1 {
                            structured_result(frame)
                        } else {
                            &Value::Null
                        };
                        apply_result(
                            &mut item.content,
                            content_text(&block["content"]),
                            block["is_error"].as_bool() == Some(true),
                            self.edit_tools
                                .get(&ScopedId::new(child, &tool_id))
                                .map(String::as_str),
                            self.tool_inputs
                                .get(&ScopedId::new(child, &tool_id))
                                .map(String::as_str),
                            structured,
                        );
                        if let Some(task) = structured["backgroundTaskId"].as_str() {
                            self.foreground_tasks.retain(|known| known != task);
                            self.background_launch(&tool_id, history, &mut item.content);
                            if item.content.status() == Some(ToolStatus::Running) {
                                self.launched(ScopedId::new(child, &tool_id));
                                self.command_task_call(task, &tool_id);
                            }
                        }
                        if matches!(&item.content, ItemContent::Tool { name, .. } if name == "Agent")
                        {
                            // A nested agent's progress never reaches this
                            // conversation, so only its launch is known.
                            let launched = child.is_none() && async_launch(frame);
                            let ended = self.ended_agent(&tool_id);
                            if launched
                                && ended.is_none() {
                                    self.background_agents.insert(tool_id.clone());
                                }
                            let background = self.background_agents.contains(&tool_id);
                            let status = if let Some(ended) =
                                ended.filter(|ended| launched || *ended == ToolStatus::Stopped)
                            {
                                ended
                            } else if background
                                && item.content.status() == Some(ToolStatus::Succeeded)
                            {
                                ToolStatus::Running
                            } else {
                                item.content.status().unwrap_or(ToolStatus::Running)
                            };
                            step.events.push(Event::Subagent {
                                model: None,
                                waiting: None,
                                detail: (status == ToolStatus::Failed).then(|| {
                                    if let ItemContent::Tool { output, .. } = &item.content {
                                        bounded_failure_detail([output.as_str()])
                                    } else {
                                        "The agent failed without reporting a reason".into()
                                    }
                                }),
                                id: tool_id.clone(),
                                kind: String::new(),
                                description: String::new(),
                                status: Some(status),
                                // Claude Code may move an agent it started in
                                // the foreground there unasked.
                                background: background.then_some(true),
                            });
                            if let ItemContent::Tool { output, .. } = &item.content
                                && let Some(address) = agent_address(structured, output)
                            {
                                if !history
                                    && child.is_none()
                                    && let Some(previous) = self.agent_call(&address, &tool_id)
                                {
                                    step.events.push(subagent_status(
                                        previous,
                                        ToolStatus::Ended,
                                        Some("Superseded by a later run of this agent"),
                                        false,
                                    ));
                                }
                                if self.addressed_agents.insert(tool_id.clone()) {
                                    step.events.push(Event::AgentAddress(AgentAddress {
                                        address,
                                        subagent: tool_id.clone(),
                                    }));
                                }
                            }
                        }
                        if let Some(content) = block["content"].as_array() {
                            for (index, attachment) in content
                                .iter()
                                .enumerate()
                                .filter(|(_, value)| string(value, "type") == "image")
                            {
                                step.events.push(Event::Item(Item {
                                    id: format!("{}:image:{index}", item.id),
                                    role: Role::Assistant,
                                    content: image_content(attachment),
                                    started_at: None,
                                    at: None,
                                }));
                            }
                        }
                        self.tool(child, item.clone());
                        step.events.push(Event::Item(item));
                    }
                    "image" => step.events.push(Event::Item(Item {
                        id: format!("{id}:{index}"),
                        role: content_role,
                        content: match image_content(block) {
                            ItemContent::Image {
                                path, description, ..
                            } => ItemContent::Image {
                                path: if unavailable.is_some() {
                                    String::new()
                                } else {
                                    path
                                },
                                description: unavailable.cloned().unwrap_or_else(|| {
                                    reference
                                        .as_ref()
                                        .map(|reference| reference.label())
                                        .unwrap_or(description)
                                }),
                                reference,
                            },
                            content => content,
                        },
                        started_at: None,
                        at: None,
                    })),
                    "document" => step.events.push(Event::Item(Item {
                        id: format!("{id}:{index}"),
                        role: content_role,
                        content: ItemContent::Image {
                            reference: None,
                            path: String::new(),
                            description: format!("Claude document: {}", string(block, "title")),
                        },
                        started_at: None,
                        at: None,
                    })),
                    other => {
                        self.warn_unknown("content", other);
                        if !history && child.is_none() && role == Role::Assistant {
                            self.turn_unsupported_content = true;
                        }
                    }
                }
            }
        }
        if role == Role::Assistant {
            if !history
                && child.is_none()
                && step.events[event_start..]
                    .iter()
                    .any(|event| matches!(event, Event::Item(_)))
            {
                self.turn_visible_content = true;
            }
            if frame["error"].as_str().is_some() {
                for event in &mut step.events[event_start..] {
                    if let Event::Item(item) = event {
                        chat::failure::mark_provider_error(Provider::Claude, item);
                    }
                }
            }
            if child.is_none()
                && let Some(usage) = frame["context_usage"].as_object()
            {
                self.usage.context_tokens = usage
                    .get("totalTokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.usage.context_tokens);
                self.usage.context_limit = usage
                    .get("maxTokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.usage.context_limit);
                step.events.push(Event::Usage(self.usage.clone()));
            }
            if let Some(error) = frame["error"].as_str() {
                step.events.push(Event::Error {
                    message: {
                        let body = content_text(&message["content"]);
                        if body.is_empty() {
                            format!("Claude: {error}")
                        } else {
                            format!("Claude: {error}\n{body}")
                        }
                    },
                    fatal: false,
                });
            }
            if child.is_none() {
                self.update_usage(&message["usage"], step);
            }
        }
        let ids = step.events[event_start..]
            .iter()
            .filter_map(|event| match event {
                Event::Item(item) => Some(item.id.clone()),
                _ => None,
            })
            .collect();
        if let Some(parent) = child {
            step.events.push(Event::Subagent {
                model: None,
                waiting: None,
                detail: None,
                id: parent.into(),
                // The Agent call's label, once published, outranks what a
                // child's frame may carry; an empty kind leaves it in place.
                kind: if self.labelled_agents.contains(parent) {
                    String::new()
                } else {
                    agent_label(
                        "",
                        string(frame, "task_description"),
                        string(frame, "subagent_type"),
                    )
                    .into()
                },
                description: string(frame, "task_description").into(),
                status: None,
                background: None,
            });
            route_subagent_events(&mut step.events, event_start, parent);
        }
        self.track_carrier(carrier, ids, child);
        // System task rows belong to the main transcript even if the Agent
        // tool arrives in a nested transcript. Emit their removals only after
        // routing that tool's own events to its parent agent.
        if !global_task_removals.is_empty() {
            step.events.push(Event::ItemsRemoved {
                ids: global_task_removals,
            });
        }
        Ok(())
    }
    /// Enrich a notification with the original Bash description and command,
    /// when its unambiguous tool-use identity still names that call.
    fn command_delivery(
        &self,
        delivery: &mut ItemContent,
        tool: &str,
        status: &str,
        summary: &str,
    ) {
        let Some(key) = self.task_call(tool) else {
            return;
        };
        let Some(Item {
            content:
                ItemContent::Command {
                    label,
                    command,
                    action: CommandAction::Run,
                    ..
                },
            ..
        }) = self
            .tools
            .get(&key)
            .or_else(|| self.evicted_launches.get(&key))
        else {
            return;
        };
        let Some((heading, detail)) = command_notification(summary, status, label, command, true)
        else {
            return;
        };
        if let ItemContent::Delivery {
            kind: DeliveryKind::Notice,
            summary,
            body,
            ..
        } = delivery
        {
            *summary = heading;
            *body = detail;
        }
    }

    fn deliveries(&self, frame: &Value) -> Vec<ItemContent> {
        let mut delivered = delivery(frame);
        if let Some(content) = recorded_notification(frame) {
            for (delivery, head) in delivered.iter_mut().zip(notification_heads(&content)) {
                self.command_delivery(
                    delivery,
                    &notification_field(head, "tool-use-id").unwrap_or_default(),
                    &notification_field(head, "status").unwrap_or_default(),
                    &notification_field(head, "summary").unwrap_or_default(),
                );
            }
        }
        delivered
    }

    /// The one call, in any transcript, a task's `tool_use_id` names, unless
    /// that call starts or resumes a subagent. An Agent's task is its
    /// subagent, and a resumed agent's task names the `SendMessage` that
    /// resumed it: both report on the agent, not on work the call started.
    ///
    /// A background command the tool cache let go of while it ran is still
    /// its call.
    fn task_call(&self, tool: &str) -> Option<ScopedId> {
        let mut calls = self.tools.keys().filter(|key| key.id == tool);
        let Some(call) = calls.next() else {
            let mut evicted = self.evicted_launches.keys().filter(|key| key.id == tool);
            let call = evicted.next()?;
            return evicted.next().is_none().then(|| call.clone());
        };
        (calls.next().is_none()
            && !matches!(
                &self.tools[call].content,
                ItemContent::Tool { name, .. } if name == "Agent" || name == "SendMessage"
            ))
        .then(|| call.clone())
    }

    /// A shell command Claude Code moved to the background is acknowledged
    /// at once, with the task's ID and the file its output goes to, which are
    /// the model's to use. The command has not finished: it runs until its
    /// task's notification settles it, which a replayed history may already
    /// hold.
    fn background_launch(&self, tool: &str, history: bool, content: &mut ItemContent) {
        let ItemContent::Command {
            output,
            status: status @ ToolStatus::Succeeded,
            action: CommandAction::Run,
            ..
        } = content
        else {
            return;
        };
        output.clear();
        *status = ToolStatus::Running;
        let outcome = self
            .task_outcomes
            .iter()
            .find(|(call, _)| call == tool)
            .map(|(_, outcome)| outcome)
            .or_else(|| self.history_outcomes.get(tool));
        match outcome {
            Some(outcome) => outcome.settle(content),
            // A history that never recorded the outcome cannot be waited on:
            // the Claude Code process that ran the command ended, and its
            // background commands with it. How the command went is unknown,
            // so it is not shown as having succeeded.
            None if history => {
                *status = ToolStatus::Stopped;
                output.push_str(UNRECORDED_BACKGROUND_RESULT);
            }
            None => {}
        }
    }

    /// Settle the call a background task's notification names, wherever it
    /// is shown, and keep the outcome for a launch acknowledgement still to
    /// come.
    fn settle_task_call(&mut self, tool: &str, outcome: TaskOutcome, step: &mut Step) {
        self.remember_outcome(tool, outcome.clone());
        let Some(key) = self.task_call(tool) else {
            return;
        };
        let Some(mut item) = self
            .tools
            .get(&key)
            .or_else(|| self.evicted_launches.get(&key))
            .cloned()
        else {
            return;
        };
        self.launches.retain(|known| known != &key);
        self.evicted_launches.remove(&key);
        let before = item.content.clone();
        outcome.settle(&mut item.content);
        if item.content == before {
            return;
        }
        self.tool(key.parent.as_deref(), item.clone());
        step.events.push(match key.parent {
            Some(parent) => Event::SubagentItem { id: parent, item },
            None => Event::Item(item),
        });
    }

    /// Remember which call a background shell task belongs to.
    fn command_task_call(&mut self, task: &str, tool: &str) {
        self.command_task_calls.retain(|(known, _)| known != task);
        self.command_task_calls
            .push_back((task.to_owned(), tool.to_owned()));
        if self.command_task_calls.len() > MAX_TOOLS {
            self.command_task_calls.pop_front();
        }
    }

    /// Claude Code's level signal: every live background task, replacing the
    /// last set. The edge frames that start and end a task can go missing,
    /// so a task leaving the level is what says it stopped running.
    ///
    /// An agent that leaves it while background work it started is still in
    /// it has paused on that work, even when its notification only said it
    /// completed. One that leaves with none, still shown as running, ended
    /// without a notice reaching this conversation; how it went is unknown.
    /// So is how a command went that leaves it before its notice arrives.
    /// A notice that arrives later still says.
    fn background_level(&mut self, frame: &Value, step: &mut Step) {
        let Some(tasks) = frame["tasks"].as_array() else {
            return;
        };
        let live = tasks
            .iter()
            .filter_map(|task| task["task_id"].as_str())
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        // A task that is not activity, such as a watcher, keeps nothing waiting.
        let launchers = tasks
            .iter()
            .filter(|task| task["ambient"].as_bool() != Some(true))
            .filter_map(|task| task["parent_task_id"].as_str())
            .collect::<HashSet<_>>();
        let Some(previous) = self.background_level.replace(live.clone()) else {
            return;
        };
        let mut left = previous.difference(&live).cloned().collect::<Vec<_>>();
        left.sort();
        for task in left {
            match self.task_agent(&task, None, false) {
                Ok(Some(owner)) => {
                    let ended = self.ended_agent(&owner);
                    let waiting = self.waiting_agents.contains(&owner);
                    if launchers.contains(task.as_str()) {
                        if !waiting
                            && !matches!(ended, Some(ToolStatus::Stopped | ToolStatus::Failed))
                        {
                            self.update_task_agent(
                                &task,
                                Some(&owner),
                                false,
                                Some(ToolStatus::Waiting),
                                ToolStatus::Waiting,
                                Some(WAITING_ON_BACKGROUND),
                                step,
                            );
                        }
                    } else if ended.is_none() && !waiting && self.background_agents.contains(&owner)
                    {
                        self.unreported_agents.retain(|known| known != &owner);
                        self.unreported_agents.push_back(owner.clone());
                        if self.unreported_agents.len() > MAX_TOOLS {
                            self.unreported_agents.pop_front();
                        }
                        self.update_task_agent(
                            &task,
                            Some(&owner),
                            false,
                            Some(ToolStatus::Ended),
                            ToolStatus::Ended,
                            Some(ENDED_UNREPORTED),
                            step,
                        );
                    }
                }
                Ok(None) => {
                    let call = self
                        .command_task_calls
                        .iter()
                        .find(|(known, _)| known == &task)
                        .map(|(_, call)| call.clone());
                    if let Some(call) = call
                        && self.launches.iter().any(|launch| launch.id == call)
                    {
                        self.settle_task_call(&call, TaskOutcome::new(ToolStatus::Ended, ""), step);
                    }
                    self.tasks.retain(|running| running != &task);
                    self.foreground_tasks.retain(|known| known != &task);
                    let row = ScopedId::new(None, &format!("claude-task:{task}"));
                    if self.tools.get(&row).and_then(|item| item.content.status())
                        == Some(ToolStatus::Running)
                    {
                        self.settle_task_row(&task, ToolStatus::Ended, step);
                    }
                }
                Err(()) => {}
            }
        }
    }

    /// Keep how a task ended for a launch acknowledgement still to come.
    fn remember_outcome(&mut self, tool: &str, outcome: TaskOutcome) {
        self.task_outcomes.retain(|(call, _)| call != tool);
        self.task_outcomes.push_back((tool.to_owned(), outcome));
        if self.task_outcomes.len() > MAX_TOOLS {
            self.task_outcomes.pop_front();
        }
    }

    /// Remember what a frame from inside an agent, or a task's lifecycle,
    /// says runs nested, whatever depth that agent is at.
    fn observe_nested(&mut self, frame: &Value) {
        for (name, call) in spawning_calls(frame) {
            if name == "SendMessage" {
                self.nested_messages.insert(call);
            } else {
                self.nested_calls.insert(call);
            }
        }
        // An agent's task says how deep it was spawned; the lead's own
        // agents are at depth one.
        if string(frame, "subtype") == "task_started"
            && frame["spawn_depth"].as_u64().is_some_and(|depth| depth > 1)
        {
            self.nested_calls.insert(string(frame, "tool_use_id"));
        }
    }

    /// Whether a call started a conversation inside one of this
    /// conversation's agents, rather than one of its own.
    fn nested_call(&self, tool: &str) -> bool {
        !self.tools.contains_key(&ScopedId::new(None, tool))
            && (self.nested_calls.contains(tool) || self.nested_agent_tool(tool))
    }

    fn nested_agent_tool(&self, tool: &str) -> bool {
        !self.tools.contains_key(&ScopedId::new(None, tool))
            && self.tools.iter().any(|(key, item)| {
                key.id == tool
                    && key.parent.is_some()
                    && matches!(&item.content, ItemContent::Tool { name, .. } if name == "Agent")
            })
    }

    fn agent_tool(&self, tool: &str) -> bool {
        self.tools.contains_key(&ScopedId::new(None, tool)) && self.unique_tool_named(tool, "Agent")
    }

    fn unique_tool_named(&self, tool: &str, expected: &str) -> bool {
        let mut candidates = self.tools.iter().filter(|(key, _)| key.id == tool);
        candidates.next().is_some_and(
            |(_, item)| matches!(&item.content, ItemContent::Tool { name, .. } if name == expected),
        ) && candidates.next().is_none()
    }

    /// Notifications have no transcript scope. Resolve only one existing Agent,
    /// and never let a contradictory tool hint retarget a known task owner.
    fn task_agent(
        &self,
        task: &str,
        reported: Option<&str>,
        started: bool,
    ) -> Result<Option<String>, ()> {
        if reported.is_some_and(|tool| {
            self.nested_call(tool) && self.tools.keys().filter(|key| key.id == tool).count() <= 1
        }) {
            // An unscoped lifecycle notice for a helper of a worker cannot
            // promote that helper into a direct child of the lead. The helper
            // may have left the tool cache; a raw ID that more than one
            // transcript uses is ambiguous and resolves below.
            return Err(());
        }
        let launched = self
            .agent_calls
            .iter()
            .find(|(agent, _)| agent == task)
            .map(|(_, tool)| tool.as_str());
        let bindings = || {
            self.agent_task_calls
                .iter()
                .filter(|(_, known)| known == task)
        };
        let owner = launched
            .or_else(|| {
                bindings()
                    .find(|(tool, _)| self.agent_tool(tool))
                    .map(|(tool, _)| tool.as_str())
            })
            .or_else(|| bindings().next().map(|(tool, _)| tool.as_str()));
        if let Some(owner) = owner {
            if (self.tools.keys().any(|key| key.id == owner) && !self.agent_tool(owner))
                || bindings().any(|(tool, _)| {
                    tool != owner
                        && (self.agent_tool(tool)
                            || (launched.is_none() && !self.agent_tool(owner)))
                })
            {
                return Err(());
            }
            // SendMessage names its own call when it restarts an Agent. That
            // alias is valid on the start, on its already recorded task, or
            // when it is that SendMessage call: a resumed run's start need not
            // name it, but its receipt does.
            if reported.is_some_and(|tool| {
                tool != owner
                    && (self.agent_tool(tool)
                        || (!started
                            && !self.unique_tool_named(tool, "SendMessage")
                            && !bindings().any(|(known, _)| known == tool)))
            }) {
                return Err(());
            }
            // A task can arrive before its Agent tool. Retain its bounded
            // lifecycle binding without inventing that Agent's presentation.
            return Ok(Some(owner.to_owned()));
        }
        let Some(tool) = reported.filter(|tool| self.agent_tool(tool)) else {
            return Ok(None);
        };
        // A launch acknowledgement's agentId need not match the task ID that
        // later reports on it. While the reported Agent is still running in the
        // background, that acknowledgement alone does not contradict its tool.
        if !started
            && (self
                .agent_task_calls
                .iter()
                .any(|(known, bound)| known == tool && bound != task)
                || (!self.background_agents.contains(tool)
                    && self
                        .agent_calls
                        .iter()
                        .any(|(bound, known)| known == tool && bound != task)))
        {
            return Err(());
        }
        Ok(Some(tool.to_owned()))
    }

    /// Shared live-text/system lifecycle reducer. A terminal receipt preserves
    /// an earlier stop; only an actual task_started can restart that owner.
    fn update_task_agent(
        &mut self,
        task: &str,
        owner: Option<&str>,
        started: bool,
        ended: Option<ToolStatus>,
        status: ToolStatus,
        detail: Option<&str>,
        step: &mut Step,
    ) -> ToolStatus {
        let Some(tool) = owner else { return status };
        // The level ended this agent unseen; its notice now says how it went.
        let reported = (started || ended.is_some_and(|ended| ended != ToolStatus::Ended))
            && self
                .unreported_agents
                .iter()
                .position(|known| known == tool)
                .and_then(|index| self.unreported_agents.remove(index))
                .is_some();
        let background = self.background_agents.contains(tool)
            || self
                .agent_calls
                .iter()
                .any(|(agent, known)| agent == task && known == tool);
        if ended == Some(ToolStatus::Waiting) {
            if self.ended_agent(tool) == Some(ToolStatus::Stopped) {
                return ToolStatus::Stopped;
            }
            // It's alive, so it stays background work that a Stop ends and that
            // a later notice settles, whatever an earlier notice said.
            self.tasks.retain(|running| running != task);
            self.ended_agents.retain(|(known, _)| known != tool);
            self.background_agents.insert(tool.into());
            if self.agent_tool(tool) || background {
                step.events.push(subagent_status(
                    tool.into(),
                    ToolStatus::Waiting,
                    detail,
                    true,
                ));
            }
            return ToolStatus::Waiting;
        }
        let status = if started {
            self.ended_agents.retain(|(known, _)| known != tool);
            if self
                .tools
                .get(&ScopedId::new(None, tool))
                .and_then(|item| item.content.status())
                .is_some_and(|status| status != ToolStatus::Running)
                && !self.restarted_agents.iter().any(|known| known == tool) {
                    self.restarted_agents.push_back(tool.into());
                    if self.restarted_agents.len() > MAX_TOOLS {
                        self.restarted_agents.pop_front();
                    }
                }
            ToolStatus::Running
        } else if ended.is_some() && self.ended_agent(tool) == Some(ToolStatus::Stopped) {
            ToolStatus::Stopped
        } else if ended.is_none()
            && let Some(prior) = self.ended_agent(tool)
        {
            // A late progress frame doesn't reopen what has ended.
            prior
        } else {
            status
        };
        if !started && ended.is_some() {
            self.tasks.retain(|running| running != task);
            if self.ended_agent(tool) != Some(ToolStatus::Stopped) {
                self.ended_agents.retain(|(known, _)| known != tool);
                self.ended_agents.push_back((tool.into(), status));
                if self.ended_agents.len() > MAX_TOOLS {
                    self.ended_agents.pop_front();
                }
            }
        }
        // The child's own result already settled this Agent with its exact
        // diagnostic. Repeating the same terminal status would clear it.
        let settled = !started
            && !reported
            && ended.is_some()
            && !self.background_agents.contains(tool)
            && !self.restarted_agents.iter().any(|known| known == tool)
            && self
                .tools
                .get(&ScopedId::new(None, tool))
                .and_then(|item| item.content.status())
                == Some(status);
        if !settled
            && (self.agent_tool(tool) || background)
            && (background || started || ended.is_some())
        {
            step.events.push(Event::Subagent {
                model: None,
                waiting: None,
                // A new run or a sign of life replaces what the last run said.
                detail: detail.map(str::to_owned).or_else(|| {
                    (started || reported || self.waiting_agents.contains(tool)).then(String::new)
                }),
                id: tool.into(),
                kind: String::new(),
                description: String::new(),
                status: Some(status),
                background: background.then_some(true),
            });
            if background && status == ToolStatus::Running {
                self.background_agents.insert(tool.into());
            } else if status != ToolStatus::Running {
                self.background_agents.remove(tool);
                self.restarted_agents.retain(|known| known != tool);
            }
        }
        status
    }

    /// A notice naming a call its task has since moved on from describes the
    /// earlier run, not the one now going.
    fn notice_predates_restart(&self, reported: &str, task: &str) -> bool {
        let newest = self
            .agent_task_calls
            .iter()
            .rev()
            .find(|(_, known)| known == task)
            .map(|(tool, _)| tool.as_str());
        newest.is_some_and(|newest| newest != reported)
            && self
                .agent_task_calls
                .iter()
                .any(|(tool, known)| tool == reported && known == task)
    }

    /// Mark a task's own row, if it has one, as ended. A stop stays a stop.
    fn settle_task_row(&mut self, task: &str, status: ToolStatus, step: &mut Step) {
        let id = format!("claude-task:{task}");
        let visible = self.visible_task_rows.contains(&id);
        // A Bash task's row is a command; an agent's or workflow's is a tool.
        if let Some(item) = self.tools.get_mut(&ScopedId::new(None, &id))
            && let Some(current) = item.content.status_mut()
        {
            if *current != ToolStatus::Stopped {
                *current = status;
            }
            if visible {
                step.events.push(Event::Item(item.clone()));
            }
        }
    }

    /// Claude Code can report a finished task only as a user-role notification.
    /// Settle what it names, so that a session which never receives the
    /// `system` form does not show that task running forever.
    fn settle_live_notifications(&mut self, frame: &Value, step: &mut Step) {
        let Some(content) = recorded_notification(frame) else {
            return;
        };
        for (tool, outcome) in notified_calls(&content) {
            if self.task_call(&tool).is_some() {
                self.settle_task_call(&tool, outcome, step);
            }
        }
        for (reported, task, notice) in live_notifications(&content) {
            if task.is_empty() {
                continue;
            }
            let (status, detail) = match notice {
                TaskNotice::Ended(status) => (status, None),
                TaskNotice::Elsewhere => (ToolStatus::Stopped, Some(REPORTS_ELSEWHERE)),
                TaskNotice::Waiting => (ToolStatus::Waiting, Some(WAITING_ON_BACKGROUND)),
                TaskNotice::Running => (ToolStatus::Running, None),
            };
            // Only an Agent's notice settles its Agent, and a contradictory
            // identity settles nothing. Other tasks, such as a Monitor, notify
            // while they still run, and an unmatched advisory stays a Delivery.
            let owner = match self.task_agent(
                &task,
                (!reported.is_empty()).then_some(reported.as_str()),
                false,
            ) {
                Ok(Some(owner)) => owner,
                Ok(None) => {
                    // A Bash or workflow task has no Agent to settle, only its row.
                    if status != ToolStatus::Running {
                        self.tasks.retain(|running| running != &task);
                        self.settle_task_row(&task, status, step);
                    }
                    continue;
                }
                Err(()) => continue,
            };
            if self.notice_predates_restart(&reported, &task) {
                continue;
            }
            // A check-in about work already running changes nothing, unless
            // the Agent was last reported as waiting: then it has resumed.
            let running = notice == TaskNotice::Running;
            if running && !self.waiting_agents.contains(&owner) {
                continue;
            }
            let id = format!("claude-task:{task}");
            let key = ScopedId::new(None, &id);
            let failure_detail = (status == ToolStatus::Failed).then(|| {
                let element = notification_elements(&content).into_iter().find(|element| {
                    notification_tasks(element)
                        .iter()
                        .any(|known| known == &task)
                });
                element
                    .map(notification_failure_detail)
                    .unwrap_or_else(|| "The agent failed without reporting a reason".into())
            });
            let status = self.update_task_agent(
                &task,
                Some(&owner),
                false,
                (!running).then_some(status),
                status,
                failure_detail.as_deref().or(detail),
                step,
            );
            if !running {
                self.tasks.retain(|running| running != &task);
            }
            let represented = self.agent_tool(&owner);
            if let Some(item) = self.tools.get_mut(&key)
                && let ItemContent::Tool {
                    status: current, ..
                } = &mut item.content
            {
                *current = status;
                if represented {
                    if self.visible_task_rows.remove(&id) {
                        step.events.push(Event::ItemsRemoved { ids: vec![id] });
                    }
                } else if self.visible_task_rows.contains(&id) {
                    step.events.push(Event::Item(item.clone()));
                }
            }
        }
    }

    /// Keep the waiting set in step with what this frame reported, and treat
    /// anything a waiting agent's own conversation produced as it resuming.
    fn reconcile_waiting_agents(&mut self, step: &mut Step) {
        let mut resumed = Vec::new();
        for event in &step.events {
            match event {
                Event::Subagent {
                    id,
                    status: Some(status),
                    ..
                } => {
                    resumed.retain(|known| known != id);
                    if *status == ToolStatus::Waiting {
                        self.waiting_agents.insert(id.clone());
                    } else {
                        self.waiting_agents.remove(id);
                    }
                }
                Event::SubagentItem { id, .. } | Event::SubagentTextDelta { id, .. }
                    if self.waiting_agents.contains(id) && !resumed.contains(id) =>
                {
                    resumed.push(id.clone());
                }
                _ => {}
            }
        }
        for id in resumed {
            self.waiting_agents.remove(&id);
            step.events
                .push(subagent_status(id, ToolStatus::Running, Some(""), true));
        }
    }

    fn is_superseded(&self, tool: &str) -> bool {
        self.superseded_agents.iter().any(|known| known == tool)
    }

    fn agent_call(&mut self, agent: &str, tool: &str) -> Option<String> {
        if self.is_superseded(tool) {
            return None;
        }
        let previous = self
            .agent_calls
            .iter()
            .rev()
            .find(|(known, old)| known == agent && old != tool)
            .map(|(_, old)| old.clone());
        if let Some(previous) = &previous {
            self.superseded_agents.push_back(previous.clone());
            if self.superseded_agents.len() > MAX_TOOLS {
                self.superseded_agents.pop_front();
            }
            self.background_agents.remove(previous);
            self.waiting_agents.remove(previous);
            self.restarted_agents.retain(|known| known != previous);
            self.ended_agents.retain(|(known, _)| known != previous);
            self.ended_agents
                .push_back((previous.clone(), ToolStatus::Ended));
            let prior_tasks = self
                .agent_task_calls
                .iter()
                .filter(|(known, _)| known == previous)
                .map(|(_, task)| task.clone())
                .collect::<Vec<_>>();
            self.agent_task_calls.retain(|(known, _)| known != previous);
            for task in prior_tasks {
                if !self.agent_task_calls.iter().any(|(known, _)| known == tool) {
                    self.agent_task_calls.push_back((tool.into(), task));
                }
            }
        }
        self.agent_calls.retain(|(known, _)| known != agent);
        self.agent_calls.push_back((agent.into(), tool.into()));
        if self.agent_calls.len() > MAX_TOOLS {
            self.agent_calls.pop_front();
        }
        previous
    }
    fn ended_agent(&self, tool: &str) -> Option<ToolStatus> {
        self.ended_agents
            .iter()
            .find(|(known, _)| known == tool)
            .map(|(_, status)| *status)
    }
    fn track_carrier(&mut self, carrier: &str, ids: Vec<String>, parent: Option<&str>) {
        if !self.carrier_items.contains_key(carrier) {
            self.carrier_order.push_back(carrier.into());
        }
        self.carrier_items.insert(carrier.into(), ids);
        if let Some(parent) = parent {
            self.carrier_parent.insert(carrier.into(), parent.into());
        }
        while self.carrier_order.len() > 2048 {
            if let Some(old) = self.carrier_order.pop_front() {
                self.carrier_items.remove(&old);
                self.carrier_parent.remove(&old);
                self.carrier_offsets.remove(&old);
            }
        }
    }
    fn update_usage(&mut self, value: &Value, step: &mut Step) {
        if !value.is_object() {
            return;
        }
        if let Some(n) = value["input_tokens"].as_u64() {
            self.usage.input_tokens = n;
            self.cache_read_tokens = value["cache_read_input_tokens"].as_u64().unwrap_or(0);
            self.cache_write_tokens = value["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        }
        if let Some(n) = value["output_tokens"].as_u64() {
            self.usage.output_tokens = n;
        }
        self.usage.context_tokens = self
            .usage
            .input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens);
        step.events.push(Event::Usage(self.usage.clone()));
    }
    fn retract(&mut self, value: &Value, step: &mut Step) -> Result<(), String> {
        let Some(carriers) = value.as_array() else {
            return Ok(());
        };
        let mut ids = Vec::new();
        let mut child_ids: HashMap<String, Vec<String>> = HashMap::new();
        for carrier in carriers {
            let carrier = carrier
                .as_str()
                .ok_or("Claude supersedes contains a non-string ID")?;
            if self.retracted.contains(carrier) {
                continue;
            }
            let Some(items) = self.carrier_items.get(carrier) else {
                // A retraction is presentation fidelity, not correctness: a carrier that
                // aged out of the correlation window must never end the conversation.
                step.events.push(Event::Error {
                    message: "Claude retracted a message outside the retained correlation window; its original text stays displayed until the session is reloaded".into(),
                    fatal: false,
                });
                self.remember_retracted(carrier);
                continue;
            };
            if let Some(parent) = self.carrier_parent.get(carrier) {
                child_ids
                    .entry(parent.clone())
                    .or_default()
                    .extend(items.iter().cloned());
            } else {
                ids.extend(items.iter().cloned());
            }
            self.remember_retracted(carrier);
        }
        for (parent, removed) in std::iter::once((None, &ids)).chain(
            child_ids
                .iter()
                .map(|(parent, ids)| (Some(parent.as_str()), ids)),
        ) {
            for id in removed {
                let key = ScopedId::new(parent, id);
                self.forget_tool(&key);
                self.tool_order.retain(|known| known != &key);
                self.blocks.retain(|(message, _), block| {
                    message.parent.as_deref() != parent || &block.item.id != id
                });
            }
        }
        if !ids.is_empty() {
            step.events.push(Event::ItemsRemoved { ids });
        }
        for (id, ids) in child_ids {
            step.events.push(Event::SubagentItemsRemoved { id, ids });
        }
        Ok(())
    }
    /// Records a hand-back's sender, and reduces the notification of an agent
    /// that already handed its report back to a notice, so the report is
    /// shown once. A hand-back is the one report without a summary; a
    /// notification's always has one.
    fn deduplicate(&mut self, mut delivery: ItemContent) -> ItemContent {
        if let ItemContent::Delivery {
            kind,
            from,
            sender,
            summary,
            body,
        } = &mut delivery
            && *kind == DeliveryKind::Report
            && !sender.is_empty()
        {
            if summary.is_empty() {
                if !self.handed_back.contains(sender) {
                    self.handed_back.push_back(sender.clone());
                }
                if self.handed_back.len() > MAX_TOOLS {
                    self.handed_back.pop_front();
                }
            } else if from.is_empty()
                && let Some(index) = self.handed_back.iter().position(|known| known == sender)
            {
                // The harness may notify of an agent again, once it is sent
                // more work; the report that work ends in is a new one.
                self.handed_back.remove(index);
                *kind = DeliveryKind::Notice;
                body.clear();
            }
        }
        delivery
    }

    /// Keep a replay of a retracted carrier hidden. The oldest are forgotten
    /// first; one replayed that late reappears instead of ending the session.
    fn remember_retracted(&mut self, carrier: &str) {
        if self.retracted.insert(carrier.into()) {
            self.retracted_order.push_back(carrier.into());
        }
        while self.retracted_order.len() > 2048 {
            if let Some(old) = self.retracted_order.pop_front() {
                self.retracted.remove(&old);
            }
        }
    }
}

impl Adapter for Claude {
    fn prepared_launch_identity(&self) -> Option<PreparedIdentity> {
        self.prepared_identity.clone()
    }

    fn command_for_run(
        &mut self,
        run: crate::model::RunId,
        command: Command,
    ) -> Result<Step, String> {
        if self.execution.is_some()
            || self.busy
            || !matches!(
                &command,
                Command::Prompt {
                    mode: PromptMode::Send,
                    ..
                }
            )
        {
            return Err("A correlated Claude prompt requires an idle conversation".into());
        }
        let mut step = self.command(command)?;
        let key = step
            .writes
            .iter()
            .find_map(|frame| {
                (frame["type"] == "user")
                    .then(|| frame["uuid"].as_str())
                    .flatten()
            })
            .ok_or("Claude did not assign an identity to the prompt")?
            .to_owned();
        self.execution = Some(run);
        step.events.insert(
            0,
            Event::Execution {
                run,
                event: crate::agent_execution::ProviderEvent::Started {
                    key: format!("claude:{key}"),
                },
            },
        );
        Ok(step)
    }
    fn initialization_pending(&self) -> bool {
        !self.history_only && !self.ready
    }
    fn outcome_pending(&self) -> bool {
        !self.deferred_attention.is_empty()
    }
    fn initialize(&mut self) -> Result<Step, String> {
        self.initialize_for_start()
            .map_err(|error| error.to_string())
    }
    fn initialize_for_start(&mut self) -> Result<Step, super::SetupError> {
        let mut step = if self.history_only {
            Step::default()
        } else {
            self.control(
                json!({"subtype":"initialize", "forwardSubagentText":true, "supportedDialogKinds":[], "promptSuggestions":true}),
            ).map_err(super::SetupError::Invalid)?
        };
        if let Some(error) = self.history_error.take() {
            step.events.push(Event::History {
                items: vec![history_notice(
                    "Earlier messages in this conversation could not be shown",
                    format!(
                        "cagents could not read this conversation's saved transcript, so its earlier messages are not shown here. Claude Code reads the transcript itself when it resumes, so it may still have the earlier context.\n\nDetails: {error}"
                    ),
                )],
                has_earlier: false,
            });
        }
        if let Some(mut history) = self.history.take() {
            history.settle_stopped();
            if let Some(body) = skipped_records(
                history.damaged,
                history.oversized,
                history.marked,
                !self.history_only,
            ) {
                step.events.push(Event::History {
                    items: vec![history_notice("Some saved messages are not shown", body)],
                    has_earlier: false,
                });
            }
            self.history_outcomes = std::mem::take(&mut history.call_outcomes);
            self.background_agents.extend(
                history
                    .background_agent_tools
                    .iter()
                    .filter(|id| history.agent_status.get(*id) == Some(&ToolStatus::Running))
                    .cloned(),
            );
            let frames = history.page_for_start()?;
            let mut history_step = Step::default();
            for frame in frames {
                self.history_message(&frame, &mut history_step)
                    .map_err(super::SetupError::Invalid)?;
            }
            let mut calls = history.agent_addresses.iter().collect::<Vec<_>>();
            calls.sort_by_key(|(tool, _)| history.agent_tools.get(*tool));
            for (tool, agent) in calls {
                self.agent_call(agent, tool);
            }
            // Replayed Agent tool uses can reinsert completed background jobs.
            // The full history index is authoritative even when a sidecar is
            // absent or fell outside the bounded child-history window.
            self.background_agents
                .retain(|id| history.agent_status.get(id) == Some(&ToolStatus::Running));
            self.waiting_agents.clear();
            // Every launch the full history recorded, including those before
            // the page shown, so a later message to one still names it.
            let mut addresses = history.agent_addresses.iter().collect::<Vec<_>>();
            addresses.sort_by_key(|(tool, _)| history.agent_tools.get(*tool));
            for (tool, agent) in addresses {
                if self.addressed_agents.insert(tool.clone()) {
                    history_step.events.push(Event::AgentAddress(AgentAddress {
                        address: agent.clone(),
                        subagent: tool.clone(),
                    }));
                }
            }
            let mut children = history.subagent_frames().unwrap_or_default();
            // An agent's own background commands are settled by the
            // notifications its sidecar recorded, which follow their launches.
            for (tool, outcome) in children
                .iter_mut()
                .flat_map(|(.., outcomes)| std::mem::take(outcomes))
            {
                if !history.agent_tools.contains_key(&tool) {
                    self.history_outcomes.insert(tool, outcome);
                }
            }
            for (parent, kind, description, frames, _) in children {
                history_step.events.push(Event::Subagent {
                    model: None,
                    waiting: None,
                    detail: None,
                    background: None,
                    id: parent.clone(),
                    kind,
                    description,
                    status: history.agent_status.get(&parent).copied(),
                });
                for mut frame in frames {
                    frame["parent_tool_use_id"] = json!(parent);
                    self.observe_nested(&frame);
                    let mut child_step = Step::default();
                    if self
                        .message_with_tools(&frame, &mut child_step, &HashMap::new(), true)
                        .is_ok()
                    {
                        history_step.events.extend(child_step.events);
                    }
                }
            }
            let failure_details = history_step
                .events
                .iter()
                .filter_map(|event| {
                    if let Event::Subagent {
                        id,
                        status: Some(ToolStatus::Failed),
                        detail: Some(detail),
                        ..
                    } = event
                    {
                        Some((id.clone(), detail.clone()))
                    } else {
                        None
                    }
                })
                .collect::<HashMap<_, _>>();
            let mut statuses = history.agent_status.iter().collect::<Vec<_>>();
            statuses.sort_by_key(|(id, _)| history.agent_tools.get(*id));
            for (id, status) in statuses {
                let status = if self.is_superseded(id) {
                    &ToolStatus::Ended
                } else {
                    status
                };
                if *status == ToolStatus::Stopped {
                    self.ended_agents.push_back((id.clone(), *status));
                } else if *status == ToolStatus::Ended {
                    self.unconfirmed_agents.insert(id.clone());
                } else if *status == ToolStatus::Failed {
                    self.recent_failed_agents.push_back(id.clone());
                }
                history_step.events.push(Event::Subagent {
                    model: None,
                    waiting: None,
                    detail: if *status == ToolStatus::Ended {
                        Some(
                            "Earlier run ended; see its transcript for the recorded outcome".into(),
                        )
                    } else if *status == ToolStatus::Failed {
                        failure_details.get(id).cloned()
                    } else {
                        None
                    },
                    id: id.clone(),
                    kind: String::new(),
                    description: String::new(),
                    status: Some(*status),
                    // Replayed launches say whether an agent ran in the
                    // background, but this process never started it, so its
                    // interrupt cannot stop it either.
                    background: Some(false),
                });
            }
            let mut items = Vec::new();
            let mut children = Vec::new();
            for event in history_step.events {
                match event {
                    Event::Item(item) => items.push(item),
                    Event::Subagent { .. }
                    | Event::AgentAddress(_)
                    | Event::SubagentItem { .. }
                    | Event::SubagentTextDelta { .. }
                    | Event::SubagentItemsRemoved { .. } => children.push(event),
                    _ => {}
                }
            }
            let mut items = settled(items);
            self.stop_earlier_steps(&mut items);
            step.events.push(Event::History {
                items,
                has_earlier: history.offset > 0,
            });
            step.events.extend(children);
            self.history = Some(history);
        }
        if self.history_only {
            self.observe_title(&mut step);
            let reason = if self.forced_history {
                "Saved conversation history is read-only; no provider was started.".to_owned()
            } else {
                format!(
                    "The saved working directory {} is unavailable. Conversation history is read-only.",
                    self.cwd.display()
                )
            };
            step.events.push(Event::ReadOnlyHistory {
                reason: reason.clone(),
            });
            step.events.push(Event::Status {
                busy: false,
                detail: reason,
            });
            step.events.push(Event::Exited { code: None });
        } else {
            self.status(false, "Initializing Claude stream-json", &mut step);
        }
        Ok(step)
    }
    fn command(&mut self, command: Command) -> Result<Step, String> {
        if self.history_only {
            match command {
                Command::LoadEarlier => {}
                Command::Refresh => return Ok(Step::default()),
                _ => {
                    return Err(
                        "This saved conversation is read-only; no provider was started".into(),
                    );
                }
            }
        }
        match command {
            Command::Prompt {
                text,
                attachments,
                mode,
            } => self.send_prompt(text, attachments, mode),
            Command::RemoveQueued { id } => {
                if !self.queued.iter().any(|message| message.id == id) {
                    return Err("This Claude message is no longer queued".into());
                }
                self.control(json!({"subtype":"cancel_async_message", "message_uuid":id}))
            }
            Command::StartQueued { .. }
            | Command::SetInterruptMode { .. }
            | Command::ListBranches
            | Command::Branch { .. } => Err("This operation is not supported by Claude".into()),
            Command::Answer {
                request_id,
                action,
                answers,
            } => self.answer(request_id, action, answers),
            Command::StopSubagent { id } => {
                // Claude Code stops an agent by its task, which is the agent
                // ID. A task frame names the call that started it; an agent
                // restored from history is known only by its launch.
                let task = self
                    .agent_task_calls
                    .iter()
                    .rev()
                    .find(|(tool, _)| *tool == id)
                    .map(|(_, task)| task)
                    .or_else(|| {
                        self.agent_calls
                            .iter()
                            .rev()
                            .find(|(_, tool)| *tool == id)
                            .map(|(agent, _)| agent)
                    })
                    .cloned()
                    .ok_or("Claude has not reported a task for this subagent yet")?;
                self.control(json!({"subtype":"stop_task", "task_id":task}))
            }
            Command::Interrupt => {
                if self.interruption.is_some() {
                    return Err("Claude interruption is already pending for this turn".into());
                }
                let step = self.control(
                    json!({"subtype":"interrupt", "_cagents_turn": self.attention_generation}),
                )?;
                self.interruption = self.busy.then_some(false);
                Ok(step)
            }
            Command::SetModel { model, reasoning } => {
                if !self.ready {
                    return Err("Claude is still initializing".into());
                }
                let advertised = self
                    .models
                    .iter()
                    .find(|m| m.id == model)
                    .ok_or("Model is not advertised by this Claude session")?;
                if reasoning
                    .as_ref()
                    .is_some_and(|effort| !advertised.reasoning.contains(effort))
                {
                    return Err("Reasoning effort is not advertised for this Claude model".into());
                }
                self.control(
                    json!({"subtype":"set_model", "model":model, "_cagents_reasoning":reasoning}),
                )
            }
            Command::SetFastMode { enabled } => {
                if !self.ready || !self.capabilities().fast_mode {
                    return Err("Fast mode is unavailable for this Claude session".into());
                }
                if self
                    .controls
                    .values()
                    .any(|request| request["settings"].get("fastMode").is_some())
                {
                    return Err("Claude fast mode change is already pending".into());
                }
                self.control(
                    json!({"subtype":"apply_flag_settings", "settings":{"fastMode":enabled}}),
                )
            }
            Command::SetPermissionMode { mode } => {
                if !MODES.contains(&mode.as_str()) {
                    return Err("Unsupported Claude permission mode".into());
                }
                self.control(json!({"subtype":"set_permission_mode", "mode":mode}))
            }
            Command::DismissPermissionDenial { tool_use_id } => {
                if tool_use_id.is_empty() || tool_use_id.len() > 4096 {
                    return Err("Invalid Claude denied tool identifier".into());
                }
                if !self.dismissed_denials.contains(&tool_use_id) {
                    self.dismissed_denials.push_back(tool_use_id.clone());
                    if self.dismissed_denials.len() > chat::PermissionDenial::LIMIT {
                        self.dismissed_denials.pop_front();
                    }
                }
                Ok(Step {
                    events: vec![Event::PermissionDenialDismissed { tool_use_id }],
                    ..Step::default()
                })
            }
            Command::RefreshPermissions => {
                if self.controls.values().any(|request| {
                    string(request, "subtype") == "get_settings"
                        || request["_cagents_permissions"].as_bool() == Some(true)
                }) {
                    return Err(
                        "Claude permission settings are already being refreshed or updated".into(),
                    );
                }
                self.permissions = None;
                self.control(json!({"subtype":"get_settings"}))
            }
            Command::UpdatePermissionSettings { update } => {
                if self.controls.values().any(|request| {
                    string(request, "subtype") == "get_settings"
                        || request["_cagents_permissions"].as_bool() == Some(true)
                }) {
                    return Err(
                        "Wait for Claude to confirm the previous permission settings change".into(),
                    );
                }
                let settings = self
                    .permissions
                    .as_ref()
                    .ok_or("Refresh Claude permission settings before changing them")?
                    .update(&update)?;
                self.control(json!({"subtype":"apply_flag_settings", "settings":settings, "_cagents_permissions":true, "_cagents_permission_update":update}))
            }
            Command::Compact => {
                if !self.capabilities().compact {
                    return Err("This Claude session does not advertise /compact".into());
                }
                self.send_prompt("/compact".into(), vec![], PromptMode::Send)
            }
            Command::Slash { command } => {
                let name = command
                    .trim()
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                if !self.commands.iter().any(|c| c.name == name) {
                    return Err("Slash command is not advertised by this Claude session".into());
                }
                self.send_prompt(
                    format!("/{}", command.trim().trim_start_matches('/')),
                    vec![],
                    PromptMode::Send,
                )
            }
            Command::ReadTextCopy { .. } => {
                Err("Text copies are served by the transcript cache".into())
            }
            Command::LoadFileChanges { .. } | Command::LoadFileChangePatches { .. } => {
                Err("File changes are served by the transcript cache".into())
            }
            Command::LoadLater => Err("Newer history is served by the transcript cache".into()),
            Command::ResumeAfterLogin { .. } => {
                Err("Live login verification is available for Codex chats".into())
            }
            Command::Refresh => self.control(json!({"subtype":"get_context_usage"})),
            Command::LoadEarlier => {
                let Some(mut history) = self.history.take() else {
                    return Err("Claude has no earlier persisted history".into());
                };
                let frames = history.page();
                let has_earlier = history.offset > 0;
                self.history = Some(history);
                let mut step = Step::default();
                // An earlier page replays launches whose agents settled long
                // ago; which agents are still running is already known.
                let running = self.background_agents.clone();
                for frame in frames? {
                    self.history_message(&frame, &mut step)?;
                }
                self.background_agents = running;
                let items = step
                    .events
                    .into_iter()
                    .filter_map(|e| {
                        if let Event::Item(i) = e {
                            Some(i)
                        } else {
                            None
                        }
                    })
                    .collect();
                let mut items = settled(items);
                self.stop_earlier_steps(&mut items);
                Ok(Step {
                    events: vec![Event::History { items, has_earlier }],
                    ..Step::default()
                })
            }
        }
    }
    fn receive(&mut self, frame: Value) -> Result<Step, String> {
        let mut step = self.receive_frame(frame)?;
        for event in &mut step.events {
            if let Event::Subagent {
                id,
                status,
                waiting,
                detail,
                background,
                ..
            } = event
                && self.is_superseded(id)
            {
                *status = Some(ToolStatus::Ended);
                *waiting = Some(false);
                *background = Some(false);
                *detail = Some("Superseded by a later run of this agent".into());
            }
        }
        for event in &step.events {
            if let Event::Subagent {
                id,
                status: Some(status),
                ..
            } = event
            {
                self.recent_failed_agents.retain(|known| known != id);
                if *status == ToolStatus::Failed {
                    self.recent_failed_agents.push_back(id.clone());
                    if self.recent_failed_agents.len() > MAX_TOOLS {
                        self.recent_failed_agents.pop_front();
                    }
                }
            }
        }
        self.reconcile_waiting_agents(&mut step);
        Ok(step)
    }
}

impl Claude {
    fn receive_frame(&mut self, frame: Value) -> Result<Step, String> {
        let mut step = Step::default();
        let parent = frame["parent_tool_use_id"]
            .as_str()
            .filter(|parent| !parent.is_empty());
        if parent.is_some() || string(&frame, "type") == "system" {
            self.observe_nested(&frame);
        }
        if parent.is_some_and(|parent| {
            self.nested_call(parent)
                || (self.nested_messages.contains(parent)
                    && !self.tools.contains_key(&ScopedId::new(None, parent)))
        }) {
            // Forwarded grandchildren have no independent transcript in the
            // lead conversation. Their launch/result tool row is retained in
            // the direct worker's transcript; their own stream and outcome
            // must neither create a peer nor settle that direct worker.
            return Ok(step);
        }
        // Reject only exact previously accepted receipts, before metadata or
        // lifecycle bookkeeping. An old child failure cannot re-block fresh
        // activity; parent result and unrelated SDK frames are untouched.
        let child = frame["parent_tool_use_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty());
        let guarded = match string(&frame, "type") {
            "result" => child,
            "stream_event" => child && string(&frame["event"], "type") == "error",
            "system" => string(&frame, "subtype") == "task_notification",
            _ => false,
        };
        let receipt = if guarded {
            scoped_failure_receipt(&frame)?
        } else {
            None
        };
        if receipt
            .as_ref()
            .is_some_and(|key| self.failure_receipts.contains(key))
        {
            return Ok(step);
        }
        if frame["parent_tool_use_id"].is_null() {
            self.fast_mode_metadata(&frame, &mut step);
        }
        if let Some(run) = self.execution
            && matches!(
                string(&frame, "type"),
                "assistant" | "stream_event" | "result"
            )
            && frame["parent_tool_use_id"].is_null()
        {
            step.events.push(Event::Execution {
                run,
                event: crate::agent_execution::ProviderEvent::Scope,
            });
        }
        // Optional predictions cannot announce or replace the conversation's
        // identity. Ignore wrong-session metadata without disrupting the chat.
        if string(&frame, "type") != "prompt_suggestion"
            && let Some(id) = frame["session_id"].as_str().filter(|id| !id.is_empty())
        {
            validate_id(id)?;
            if self
                .session_id
                .as_deref()
                .is_some_and(|expected| expected != id)
            {
                return Err("Claude emitted a different session ID; refusing to join a different conversation".into());
            }
            if self.session_id.is_none() {
                self.session_id = Some(id.into());
            }
            // SessionStart hooks and other system frames name the session
            // before the CLI answers initialize. Only frames after it announce
            // the session: a resume is confirmed by that answer, and a fresh
            // conversation, as without hooks, by its first turn.
            if self.ready {
                step.events.push(Event::Session {
                    id: id.into(),
                    title: None,
                });
            }
        }
        match required(&frame, "type")? {
            "control_request" => return self.permission(frame),
            "control_cancel_request" => {
                self.retire(required(&frame, "request_id")?.into(), &mut step);
            }
            "conversation_reset" => {
                self.started_generation = None;
                self.session_id = None;
                self.history = None;
                self.pending.clear();
                self.retired.clear();
                self.streams.clear();
                self.blocks.clear();
                self.ignored_blocks.clear();
                self.carrier_offsets.clear();
                self.message_offsets.clear();
                self.message_order.clear();
                self.tools.clear();
                self.labelled_agents.clear();
                self.visible_task_rows.clear();
                self.edit_tools.clear();
                self.tool_inputs.clear();
                self.tool_input_order.clear();
                self.dismissed_denials.clear();
                self.permissions = None;
                self.tool_order.clear();
                self.tasks.clear();
                self.report_tasks.clear();
                self.command_tasks.clear();
                self.subagent_tasks.clear();
                self.foreground_tasks.clear();
                self.task_outcomes.clear();
                self.history_outcomes.clear();
                self.launches.clear();
                self.evicted_launches.clear();
                self.failure_receipts.clear();
                self.agent_task_calls.clear();
                self.ended_agents.clear();
                self.superseded_agents.clear();
                self.unconfirmed_agents.clear();
                self.recent_failed_agents.clear();
                self.accepted_user_prompts.clear();
                self.handed_back.clear();
                self.submissions.clear();
                self.queued.clear();
                self.stream_events.clear();
                self.carrier_items.clear();
                self.carrier_order.clear();
                self.retracted.clear();
                self.retracted_order.clear();
                self.usage = Usage::default();
                self.cache_read_tokens = 0;
                self.cache_write_tokens = 0;
                step.events.push(Event::Reset {
                    reason: "Claude reset the conversation".into(),
                });
            }
            "control_response" => {
                let response = &frame["response"];
                let id = required(response, "request_id")?;
                let Some(request) = self.controls.remove(id) else {
                    return Ok(step);
                };
                if string(&request, "subtype") == "interrupt"
                    && let Some(generation) = request["_cagents_turn"].as_u64()
                {
                    let accepted = string(response, "subtype") == "success";
                    if let Some((key, outcome, execution)) =
                        self.deferred_attention.remove(&generation)
                    {
                        // This acknowledgement may settle the old run after
                        // another accepted prompt has begun. Keep its exact
                        // execution result without replacing that prompt's
                        // current notification context.
                        step.suppress_current_notification = generation
                            != self.attention_generation
                            && self.started_generation == Some(self.attention_generation);
                        let outcome = if accepted {
                            crate::notifications::Kind::Cancelled
                        } else {
                            outcome
                        };
                        step.events.push(Event::TurnFinished { key, outcome });
                        if let Some(run) = execution {
                            step.events.push(Event::Execution {
                                run,
                                event: crate::agent_execution::ProviderEvent::Finished { outcome },
                            });
                        }
                    } else if generation == self.attention_generation && self.interruption.is_some()
                    {
                        self.interruption = accepted.then_some(true);
                    }
                }
                if string(response, "subtype") == "error" {
                    if string(&request, "subtype") == "cancel_async_message"
                        && let Some(id) = request["message_uuid"].as_str()
                    {
                        step.events.push(Event::QueueRemoval(chat::QueueRemoval {
                            id: id.into(),
                            removed: false,
                        }));
                    }
                    if string(&request, "subtype") == "initialize"
                        && request.get("promptSuggestions").is_some()
                    {
                        // Older CLIs may reject the optional SDK field. Retry
                        // the identical handshake once without suggestions.
                        let mut request = request.clone();
                        request.as_object_mut().unwrap().remove("promptSuggestions");
                        let retry = self.control(request)?;
                        step.writes.extend(retry.writes);
                        return Ok(step);
                    }
                    step.events.push(Event::Error {
                        message: format!(
                            "Claude {}: {}",
                            string(&request, "subtype"),
                            string(response, "error")
                        ),
                        fatal: string(&request, "subtype") == "initialize",
                    });
                } else if string(response, "subtype") != "success" {
                    return Err("Unsupported Claude control response subtype".into());
                } else {
                    match string(&request, "subtype") {
                        "initialize" => {
                            self.ready = true;
                            step.events.push(Event::Ready(true));
                            // The CLI names its session only on frames of a
                            // turn, so an idle resumed conversation would stay
                            // unconfirmed until its next prompt and could not
                            // be checkpointed or restored meanwhile. A
                            // `--resume` the CLI cannot load ends the process
                            // instead of answering, which makes this reply its
                            // confirmation of that exact conversation.
                            if self.resumed
                                && let Some(id) = &self.session_id
                            {
                                step.events.push(Event::Session {
                                    id: id.clone(),
                                    title: None,
                                });
                            }
                            if self.resumed {
                                self.observe_title(&mut step);
                            }
                            self.metadata(&response["response"], &mut step);
                            self.fast_mode_metadata(&response["response"], &mut step);
                            if let Some(mode) =
                                response["response"]["current_permission_mode"].as_str()
                            {
                                step.events.push(Event::PermissionMode(Some(
                                    if mode == "default" { "manual" } else { mode }.into(),
                                )));
                            }
                            self.status(self.busy, "Claude connected", &mut step);
                            for key in [
                                "pending_permission_requests",
                                "pending_user_dialog_requests",
                            ] {
                                if let Some(requests) = response[key].as_array() {
                                    for pending in requests {
                                        let replay = self.permission(pending.clone())?;
                                        step.writes.extend(replay.writes);
                                        step.events.extend(replay.events);
                                    }
                                }
                            }
                        }
                        "set_model" => {
                            self.selected = request["model"].as_str().map(str::to_owned);
                            if !self.busy {
                                self.running = self
                                    .selected
                                    .as_ref()
                                    .map(|id| self.resolved.get(id).unwrap_or(id).clone());
                            }
                            self.metadata(&Value::Null, &mut step);
                            let next = self.control(json!({"subtype":"apply_flag_settings", "settings":{"effortLevel":request["_cagents_reasoning"]}}))?;
                            step.writes.extend(next.writes);
                        }
                        "get_settings" => {
                            let snapshot =
                                super::claude_permissions::Snapshot::parse(&response["response"])?;
                            step.events
                                .push(Event::PermissionSettings(snapshot.settings.clone()));
                            if let Some(update) = request.get("_cagents_permission_update") {
                                let update =
                                    serde_json::from_value::<chat::PermissionSettingsUpdate>(
                                        update.clone(),
                                    )
                                    .map_err(
                                        |_| "Malformed pending Claude permission settings update",
                                    )?;
                                if let Err(message) = snapshot.confirm(&update) {
                                    step.events.push(Event::Error {
                                        message,
                                        fatal: false,
                                    });
                                }
                            }
                            self.permissions = Some(snapshot);
                        }
                        "apply_flag_settings"
                            if request["_cagents_permissions"].as_bool() == Some(true) =>
                        {
                            self.permissions = None;
                            let next = self.control(json!({"subtype":"get_settings", "_cagents_permission_update":request["_cagents_permission_update"]}))?;
                            step.writes.extend(next.writes);
                        }
                        "apply_flag_settings" if request["settings"].get("fastMode").is_some() => {
                            // Only the acknowledged session overlay changes this setting.
                            self.fast_mode = Some(chat::FastMode {
                                state: if request["settings"]["fastMode"].as_bool() == Some(true) {
                                    chat::FastModeState::On
                                } else {
                                    chat::FastModeState::Off
                                },
                                disabled_reason: None,
                            });
                            step.events
                                .push(Event::FastMode(self.fast_mode.clone().unwrap()));
                            step.events.push(Event::Capabilities(self.capabilities()));
                            self.fast_mode_metadata(&response["response"], &mut step);
                        }
                        "apply_flag_settings" => {
                            self.reasoning = request["settings"]["effortLevel"]
                                .as_str()
                                .map(str::to_owned);
                            if !self.busy {
                                self.running_reasoning = self.reasoning.clone();
                            }
                            self.metadata(&Value::Null, &mut step);
                        }
                        "set_permission_mode" => {
                            step.events
                                .push(Event::PermissionMode(Some(string(&request, "mode").into())));
                            step.events.push(Event::Status {
                                busy: self.busy,
                                detail: format!(
                                    "Claude permission mode: {}",
                                    string(&request, "mode")
                                ),
                            });
                        }
                        "interrupt" => {
                            step.events.push(Event::Status {
                                busy: self.busy,
                                detail:
                                    "Claude acknowledged interruption; waiting for turn settlement"
                                        .into(),
                            });
                        }
                        "get_context_usage" => {
                            self.usage.context_tokens = response["response"]["totalTokens"]
                                .as_u64()
                                .unwrap_or(self.usage.context_tokens);
                            self.usage.context_limit = response["response"]["maxTokens"]
                                .as_u64()
                                .unwrap_or(self.usage.context_limit);
                            step.events.push(Event::Usage(self.usage.clone()));
                        }
                        _ => {}
                    }
                }
            }
            "assistant" | "user" => {
                // The replay acknowledgement can precede consumption. Its
                // stable IDs will upsert the deferred items after delivery.
                let awaiting_delivery = string(&frame, "type") == "user"
                    && frame["uuid"].as_str().is_some_and(|id| {
                        self.submissions
                            .get(id)
                            .is_some_and(|submission| submission.deferred.is_some())
                    });
                if !awaiting_delivery {
                    if actual_user_prompt(&frame)
                        && frame["parent_tool_use_id"]
                            .as_str()
                            .is_none_or(str::is_empty)
                        && let Some(id) = frame["uuid"].as_str()
                        && !self.submissions.contains_key(id)
                    {
                        self.accept_user_prompt(id, &mut step);
                    }
                    self.message(&frame, &mut step)?;
                }
            }
            "stream_event" => {
                if !guarded && let Some(id) = frame["uuid"].as_str() {
                    if self.stream_events.iter().any(|seen| seen == id) {
                        return Ok(step);
                    }
                    self.stream_events.push_back(id.into());
                    if self.stream_events.len() > 2048 {
                        self.stream_events.pop_front();
                    }
                }
                let event_start = step.events.len();
                self.stream(&frame, &mut step)?;
                if let Some(parent) = frame["parent_tool_use_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                {
                    route_subagent_events(&mut step.events, event_start, parent);
                }
            }
            "result" => {
                // A delegated result still carries scoped permission evidence,
                // even though it must never settle the parent turn.
                for denial in frame["permission_denials"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(chat::PermissionDenial::LIMIT)
                {
                    self.permission_denied(
                        frame["parent_tool_use_id"].as_str(),
                        denial,
                        &mut step,
                    )?;
                }
                if let Some(parent) = frame["parent_tool_use_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                {
                    // A child result settles this exact delegated turn, not the
                    // parent's prompt, usage, interrupt or accepted execution.
                    let errors = content_text(&frame["errors"]);
                    let detail = if !errors.is_empty() {
                        errors
                    } else {
                        string(&frame, "result").to_owned()
                    };
                    let status = if self.ended_agent(parent) == Some(ToolStatus::Stopped) {
                        ToolStatus::Stopped
                    } else if frame["is_error"].as_bool() == Some(true) {
                        ToolStatus::Failed
                    } else if self.waiting_agents.contains(parent) {
                        // Its turn ending is what put it in this state, and
                        // only a notice can say it has finished its work.
                        ToolStatus::Waiting
                    } else {
                        ToolStatus::Succeeded
                    };
                    if status != ToolStatus::Waiting {
                        self.background_agents.remove(parent);
                    }
                    if let Some(Item {
                        content: ItemContent::Tool { status: known, .. },
                        ..
                    }) = self.tools.get_mut(&ScopedId::new(None, parent))
                    {
                        *known = status;
                    }
                    self.tasks.retain(|task| {
                        !self
                            .agent_task_calls
                            .iter()
                            .any(|(tool, known)| tool == parent && known == task)
                            && !self
                                .agent_calls
                                .iter()
                                .any(|(known, tool)| tool == parent && known == task)
                    });
                    if let Some(stream) = self.streams.remove(parent) {
                        self.settle(Some(parent), &stream.message);
                    }
                    if frame["is_error"].as_bool() == Some(true) && !detail.is_empty() {
                        step.events.push(Event::SubagentItem {
                            id: parent.into(),
                            item: child_failure_item(&frame, "result-error", detail.clone()),
                        });
                    }
                    step.events.push(Event::Subagent {
                        id: parent.into(),
                        kind: String::new(),
                        description: String::new(),
                        status: Some(status),
                        background: None,
                        model: None,
                        waiting: Some(false),
                        detail: Some(if status == ToolStatus::Waiting {
                            WAITING_ON_BACKGROUND.to_owned()
                        } else {
                            detail
                        }),
                    });
                    self.remember_failure_receipt(receipt);
                    return Ok(step);
                }
                if frame["parent_tool_use_id"].is_null() {
                    self.observe_title(&mut step);
                }
                // A successful reply must not look empty when its only content
                // could not be shown. The private content itself stays omitted.
                if std::mem::take(&mut self.turn_unsupported_content)
                    && !std::mem::take(&mut self.turn_visible_content)
                    && frame["is_error"] != true
                {
                    step.events.push(Event::Item(unsupported_content_item(
                        frame["uuid"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    )));
                }
                self.turn_visible_content = false;
                let can_suggest = frame["parent_tool_use_id"].is_null()
                    && frame["is_error"] != true
                    && frame["subtype"] == "success"
                    && self.interruption.is_none()
                    && self.started_generation == Some(self.attention_generation);
                let had_turn = self.busy;
                let interruption = self.interruption.take();
                if had_turn || frame["uuid"].as_str().is_some() {
                    step.events.push(Event::TurnSettled);
                    use crate::notifications::Kind;
                    // A delayed interrupt acknowledgement can arrive during
                    // another turn. Allocate its fallback identity now instead
                    // of borrowing that later turn's runtime identity.
                    let key = Some(format!(
                        "claude:{}",
                        frame["uuid"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
                    ));
                    let outcome = if interruption == Some(true) {
                        Kind::Cancelled
                    } else if frame["is_error"].as_bool() == Some(true) {
                        Kind::Failed
                    } else {
                        Kind::Succeeded
                    };
                    let execution = self.execution.take();
                    if let Some(run) = execution {
                        step.events.push(Event::Execution {
                            run,
                            event: crate::agent_execution::ProviderEvent::Settled,
                        });
                    }
                    if interruption == Some(false) {
                        self.deferred_attention
                            .insert(self.attention_generation, (key, outcome, execution));
                    } else {
                        step.events.push(Event::TurnFinished { key, outcome });
                        if let Some(run) = execution {
                            step.events.push(Event::Execution {
                                run,
                                event: crate::agent_execution::ProviderEvent::Finished { outcome },
                            });
                        }
                    }
                }
                self.attention_generation = self.attention_generation.wrapping_add(1);
                self.suggestion_turn = (can_suggest && self.pending.is_empty()).then(|| {
                    (
                        self.attention_generation,
                        format!("claude:{}", self.attention_generation),
                    )
                });
                // Background agents have their own status. They must not
                // prevent new input once the main turn has ended.
                let busy = !self.pending.is_empty();
                self.status(
                    busy,
                    if busy {
                        "Claude turn ended; decisions remain"
                    } else {
                        "Claude turn complete"
                    },
                    &mut step,
                );
                if let Some(stream) = self.streams.remove("") {
                    self.settle(None, &stream.message);
                }
                // Result input totals span multiple API calls; they are not the
                // context occupancy of the last individual model call.
                if let Some(tokens) = frame["usage"]["input_tokens"].as_u64() {
                    self.usage.input_tokens = tokens;
                }
                if let Some(tokens) = frame["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = tokens;
                }
                if let Some(cost) = frame["total_cost_usd"]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v >= 0.0)
                {
                    self.usage.cost_microusd = Some((cost * 1_000_000.0).round() as u64);
                }
                if let Some(models) = frame["modelUsage"].as_object() {
                    let data = self
                        .running
                        .as_ref()
                        .and_then(|m| models.get(m))
                        .or_else(|| {
                            if models.len() == 1 {
                                models.values().next()
                            } else {
                                None
                            }
                        });
                    if let Some(data) = data {
                        self.usage.context_limit = data["contextWindow"]
                            .as_u64()
                            .unwrap_or(self.usage.context_limit);
                    }
                }
                step.events.push(Event::Usage(self.usage.clone()));
                if frame["is_error"].as_bool() == Some(true) {
                    let errors = content_text(&frame["errors"]);
                    let message = if !errors.is_empty() {
                        errors
                    } else if !string(&frame, "result").is_empty() {
                        string(&frame, "result").into()
                    } else {
                        format!("Claude result: {}", string(&frame, "subtype"))
                    };
                    step.events.push(Event::Error {
                        message,
                        fatal: false,
                    });
                }
            }
            "system" => self.system(&frame, &mut step)?,
            "auth_status" => {
                let output = content_text(&frame["output"]);
                step.events.push(Event::Status {
                    busy: self.busy,
                    detail: format!("Claude authentication: {output}"),
                });
                if let Some(error) = frame["error"].as_str() {
                    step.events.push(Event::Error {
                        message: error.into(),
                        fatal: false,
                    });
                }
                for token in output.split_whitespace() {
                    if token.starts_with("https://") {
                        step.events.push(Event::OpenUrl { url:token.into(), instructions:"Complete Claude authentication in your browser; never share authentication codes in chat".into() });
                    }
                }
            }
            "command_lifecycle" => {
                // UUID-tagged user messages receive these transport lifecycle
                // notifications. Only result settles a turn and its decisions.
                let state = required(&frame, "state")?;
                if let Some(id) = frame["command_uuid"].as_str() {
                    if state == "queued" {
                        if let Some(message) = self.submissions.get(id)
                            && !self.queued.iter().any(|queued| queued.id == id)
                        {
                            self.queued.push(message.preview.clone());
                        }
                    } else if matches!(state, "started" | "completed" | "cancelled" | "discarded") {
                        if matches!(state, "started" | "completed")
                            && self.submissions.contains_key(id)
                        {
                            self.accept_user_prompt(id, &mut step);
                        }
                        if state == "started" && self.submissions.contains_key(id) {
                            self.start_turn(&mut step);
                            self.status(true, "Claude is working", &mut step);
                        }
                        self.queued.retain(|message| message.id != id);
                        if let Some(submission) = self.submissions.remove(id)
                            && let Some(items) = submission.deferred
                        {
                            if matches!(state, "started" | "completed") {
                                step.events.extend(items.into_iter().map(Event::Item));
                            } else {
                                // A delayed replay acknowledgement must not
                                // resurrect a message the provider discarded.
                                self.remember_retracted(id);
                                step.events.push(Event::QueueRemoval(chat::QueueRemoval {
                                    id: id.into(),
                                    removed: true,
                                }));
                            }
                        }
                    }
                    step.events.push(Event::QueueChanged(self.queued.clone()));
                }
                step.events.push(Event::Status {
                    busy: self.busy,
                    detail: format!("Claude command {}", required(&frame, "state")?),
                });
            }
            "tool_progress" => {
                step.events.push(Event::Status {
                    busy: self.busy,
                    detail: format!(
                        "{} running ({} seconds)",
                        string(&frame, "tool_name"),
                        frame["elapsed_time_seconds"]
                    ),
                });
            }
            "tool_use_summary" => {
                step.events.push(Event::Item(text_item(
                    required(&frame, "uuid")?.into(),
                    Role::System,
                    string(&frame, "summary").into(),
                    false,
                )));
            }
            "rate_limit_event" => {
                let info = &frame["rate_limit_info"];
                if let Some(update) = account::claude::parse_rate_limit_event(info) {
                    step.events
                        .push(Event::AccountUsage(LiveReport::Claude(update)));
                }
                let detail = rate_limit_detail(info);
                if !detail.is_empty() {
                    step.events.push(Event::Status {
                        busy: self.busy,
                        detail,
                    });
                }
            }
            "prompt_suggestion" => {
                // Claude's SDK emits at most one suggestion after each result,
                // with session identity but no turn ID. Accept only the current
                // result-delimited idle generation; new input invalidates it.
                if !self.busy
                    && frame["session_id"].as_str() == self.session_id.as_deref()
                    && self.session_id.is_some()
                    && let Some((generation, source)) = self.suggestion_turn.take()
                    && generation == self.attention_generation
                    && let Some(text) = frame["suggestion"].as_str()
                    && chat::PromptSuggestion::valid_text(text)
                {
                    step.events.push(Event::PromptSuggestion {
                        session_id: self.session_id.clone().unwrap(),
                        source,
                        text: text.into(),
                    });
                }
            }
            "keep_alive" => {}
            other => self.warn_unknown("frame", other),
        }
        self.remember_failure_receipt(receipt);
        Ok(step)
    }
}

/// A usage-limit notice as a sentence, or nothing while the limit is not
/// biting.
///
/// The frame carries a tagged object, and interpolating it put a JSON object
/// on the status line - the one row a reader watches while a turn runs. An
/// allowed request is silent for the same reason an idle thread is: a session
/// inside its limit has nothing to report. The reset time is named where the
/// frame carries one, because "wait" without "until when" is not actionable;
/// both spellings are read because the field has travelled under each.
fn rate_limit_detail(info: &Value) -> String {
    let resets = info["resets_at"]
        .as_u64()
        .or_else(|| info["resetsAt"].as_u64())
        .map(|at| format!(", resets at {}", clock_label(at * 1_000)))
        .unwrap_or_default();
    match info["status"].as_str() {
        Some("allowed") | None => String::new(),
        Some("allowed_warning") => format!("Approaching Claude's usage limit{resets}"),
        Some("rejected") => format!("Claude's usage limit is reached{resets}"),
        Some(other) => format!("Claude usage limit {other}{resets}"),
    }
}

/// Only a provider-suggested allow rule scoped to this tool action can be
/// remembered. Never accept mode changes, whole-tool rules or directory grants.
fn session_permission_suggestions(request: &Value) -> Option<Value> {
    if request["suppress_always_allow_rule"].as_bool() == Some(true) {
        return None;
    }
    let suggestions = request["permission_suggestions"].as_array()?;
    if suggestions.is_empty() || suggestions.len() > 16 {
        return None;
    }
    let tool = request["tool_name"].as_str()?;
    let mut scoped = Vec::new();
    for suggestion in suggestions {
        if string(suggestion, "type") != "addRules" || string(suggestion, "behavior") != "allow" {
            return None;
        }
        let rules = suggestion["rules"].as_array()?;
        if rules.is_empty() || rules.len() > 16 {
            return None;
        }
        for rule in rules {
            let content = rule["ruleContent"].as_str()?.trim();
            if string(rule, "toolName") != tool
                || content.is_empty()
                || content.starts_with('*')
                || matches!(content, "/**" | "/*" | "*:*" | ":*")
                || content.len() > 4096
                || content.chars().any(char::is_control)
            {
                return None;
            }
        }
        scoped.push(
            json!({"type":"addRules", "rules":rules, "behavior":"allow", "destination":"session"}),
        );
    }
    Some(Value::Array(scoped))
}

/// SDK permission reasons can carry ANSI escapes. Keep them plain on every surface.
/// What a permission request asks, as the surface sets it: the sentences
/// Claude Code gave, then what would be done as a fenced block.
///
/// The surface sets what precedes the first fence as prose and the block as
/// literal text, so the block is where a command or a patch can be read as
/// what it is. The input used to follow the sentences as one line of JSON,
/// which the surface could only set as more of the sentence. A command is
/// shown as the command, an edit as the patch it would make, and anything
/// else as its input with one argument to a line.
fn permission_description(name: &str, input: &Value, sentences: &[&str]) -> String {
    let mut question: Vec<&str> = sentences
        .iter()
        .map(|sentence| sentence.trim())
        .filter(|sentence| !sentence.is_empty())
        .collect();
    let changes = super::claude_edits::changes(name, input);
    let (language, payload) = match (&changes, name) {
        (Some(changes), _) => {
            // A patch does not say which file it is for.
            question.extend(changes.iter().map(|change| change.path.as_str()));
            (
                "diff",
                changes
                    .iter()
                    .map(|change| change.diff.as_str())
                    .collect::<String>(),
            )
        }
        (None, "Bash") if !string(input, "command").is_empty() => {
            // Claude's own account of the command, where it gave one.
            question.push(string(input, "description").trim());
            question.retain(|sentence| !sentence.is_empty());
            ("sh", string(input, "command").to_owned())
        }
        _ => (
            "json",
            serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string()),
        ),
    };
    // The surface reads the block as running from the first fence to the
    // last, so a fence inside the payload is part of it and needs no longer
    // fence around it.
    format!(
        "{}\n\n```{language}\n{}\n```",
        question.join("\n"),
        payload.trim_end_matches('\n')
    )
}

fn clean_permission_text(value: &str) -> String {
    let mut chars = value.chars().peekable();
    let mut plain = String::new();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for code in chars.by_ref() {
                        if ('@'..='~').contains(&code) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(code) = chars.next() {
                        if code == '\u{7}' || (code == '\u{1b}' && chars.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if !ch.is_control() || ch == '\n' || ch == '\t' {
            plain.push(ch);
        }
    }
    plain
}

fn choice(id: &str, label: &str) -> Choice {
    Choice {
        id: id.into(),
        label: label.into(),
        description: String::new(),
    }
}
fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}
/// What an agent goes by: the name its Agent call asked for, else the first
/// line of the short task description it was given, else its agent type.
/// Several agents share a type, such as `general-purpose`, so the type, which
/// the call's own card still shows, is the last resort.
fn agent_label<'a>(name: &'a str, description: &'a str, agent_type: &'a str) -> &'a str {
    [name, description, agent_type]
        .into_iter()
        .find_map(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
        .unwrap_or_default()
}
fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("Claude protocol frame is missing {key}"))
}
/// Explicit child API/result failures retain their original diagnostic in
/// history as well as transient status. A carrier identifies exact replay;
/// without one, distinct actual receipts must never share a fabricated ID.
fn child_failure_item(frame: &Value, source: &str, message: String) -> Item {
    let carrier = frame["uuid"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut item = text_item(
        format!("claude-{source}:{carrier}:0"),
        Role::System,
        message,
        false,
    );
    chat::failure::mark_provider_error(Provider::Claude, &mut item);
    item
}

/// Hash the already parsed frame without allocating another full diagnostic.
/// UUID-less receipts have no replay authority and are always processed.
fn scoped_failure_receipt(frame: &Value) -> Result<Option<[u8; 32]>, String> {
    if frame["uuid"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .is_none()
    {
        return Ok(None);
    }
    struct HashWriter(sha2::Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            sha2::Digest::update(&mut self.0, bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(<sha2::Sha256 as sha2::Digest>::new());
    serde_json::to_writer(&mut writer, frame).map_err(|error| error.to_string())?;
    Ok(Some(sha2::Digest::finalize(writer.0).into()))
}

/// A child frame belongs to its agent's transcript, even when Claude gives it
/// the wire role `user` for an internal prompt or tool result.
fn route_subagent_events(events: &mut Vec<Event>, start: usize, parent: &str) {
    let child: Vec<_> = events
        .drain(start..)
        .filter(|event| {
            // Helpers delegated by this worker are its own dependencies. Their
            // Agent tool rows remain in its transcript, but listing them as
            // peers double-counts the lead's direct delegated work.
            match event {
                Event::Subagent { id, .. } => id == parent,
                Event::AgentAddress(_) => false,
                Event::Status { .. }
                | Event::Usage(_)
                | Event::TurnStarted
                | Event::TurnSettled => false,
                _ => true,
            }
        })
        .map(|event| match event {
            Event::Item(mut item) => {
                if item.role == Role::User {
                    item.role = Role::System;
                }
                Event::SubagentItem {
                    id: parent.into(),
                    item,
                }
            }
            Event::TextDelta {
                id,
                role,
                thinking,
                delta,
            } => Event::SubagentTextDelta {
                id: parent.into(),
                item_id: id,
                role: if role == Role::User {
                    Role::System
                } else {
                    role
                },
                thinking,
                delta,
            },
            Event::ItemsRemoved { ids } => Event::SubagentItemsRemoved {
                id: parent.into(),
                ids,
            },
            Event::Error { message, .. } => Event::Subagent {
                id: parent.into(),
                kind: String::new(),
                description: String::new(),
                // A retryable API error is not proof that the task stopped.
                // Keep its exact status, but expose that it needs attention.
                status: None,
                background: None,
                model: None,
                waiting: Some(true),
                detail: Some(message),
            },
            event => event,
        })
        .collect();
    events.extend(child);
}
fn slash_command_prompt(content: &str) -> Option<String> {
    // The stream sends the message first; the transcript records the name
    // first, with the later elements indented.
    let mut rest = content;
    let mut element = |tag: &str| {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let (value, after) = rest
            .trim_start()
            .strip_prefix(open.as_str())?
            .split_once(close.as_str())?;
        rest = after;
        Some(value)
    };
    let (message, name) = if content.starts_with("<command-message>") {
        let message = element("command-message")?;
        (message, element("command-name")?)
    } else {
        let name = element("command-name")?;
        (element("command-message")?, name)
    };
    let args = element("command-args")?;
    if !rest.trim().is_empty() || name.strip_prefix('/')? != message || message.is_empty() {
        return None;
    }
    Some(if args.is_empty() {
        name.into()
    } else {
        format!("{name} {args}")
    })
}

/// Only a person's new prompt advances the scope of recent failures.
fn actual_user_prompt(frame: &Value) -> bool {
    if string(frame, "type") != "user"
        || model_only(frame)
        || frame["origin"]["kind"]
            .as_str()
            .is_some_and(|kind| kind != "human")
    {
        return false;
    }
    let content = &frame["message"]["content"];
    let prompt = content.is_string()
        || content.as_array().is_some_and(|blocks| {
            !blocks
                .iter()
                .any(|block| string(block, "type") == "tool_result")
                && blocks
                    .iter()
                    .any(|block| matches!(string(block, "type"), "text" | "image" | "document"))
        });
    prompt && delivery(frame).is_empty()
}

/// What a user frame Claude Code wrote on another agent's behalf delivers,
/// or nothing for a frame the user wrote.
///
/// The harness tags these frames with an `origin`: `peer` for a subagent's
/// hand-back, a teammate's message or another session's, and
/// `task-notification` for background work changing state. A frame without
/// one is recognized by the envelope the harness wraps around its content,
/// which is the only form a replayed or older frame is known to carry. A
/// frame cagents itself sent is marked `human` and never reinterpreted, so a
/// user quoting such an envelope still sees their own message.
fn delivery(frame: &Value) -> Vec<ItemContent> {
    let origin = &frame["origin"];
    let content = message_text(&frame["message"]["content"]);
    match string(origin, "kind") {
        "human" => Vec::new(),
        "peer" => {
            let mut peer = content
                .as_deref()
                .and_then(peer_envelope)
                .unwrap_or_default();
            if let Some(body) = origin["body"].as_str() {
                peer.body = body.into();
            }
            if let Some(name) = origin["name"].as_str() {
                peer.from = name.into();
            }
            if let Some(id) = origin["senderTaskId"].as_str() {
                peer.sender = id.into();
            }
            if peer.from.is_empty() {
                peer.from = string(origin, "from").into();
            }
            if peer.body.is_empty() {
                return Vec::new();
            }
            vec![peer.finish(origin["handback"].as_bool() == Some(true))]
        }
        "task-notification" => {
            let content = content.unwrap_or_default();
            let notices = task_notifications(&content);
            if notices.is_empty() && !content.trim().is_empty() {
                // Some changes are worded as plain text instead, such as the
                // user stopping background agents.
                let (summary, body) = content
                    .trim()
                    .split_once('\n')
                    .unwrap_or((content.trim(), ""));
                vec![ItemContent::Delivery {
                    kind: DeliveryKind::Notice,
                    from: String::new(),
                    sender: String::new(),
                    summary: summary.trim().into(),
                    body: body.trim().into(),
                }]
            } else {
                notices
            }
        }
        _ => {
            let Some(content) = content else {
                return Vec::new();
            };
            if let Some(peer) = peer_envelope(&content) {
                return vec![peer.finish(false)];
            }
            let teammates = teammate_messages(&content);
            if !teammates.is_empty() {
                return teammates;
            }
            // Without an origin, only a frame that is nothing but the
            // envelope is the harness's; a user may mention one in passing.
            if content.trim_start().starts_with(NOTIFICATION_OPEN) {
                task_notifications(&content)
            } else {
                Vec::new()
            }
        }
    }
}

/// Claude marks as meta what it injects for the model alone, such as a
/// skill's body. A peer's delivery is marked too, being meta to the model,
/// but is a message to the reader. The broader isSynthetic flag cannot prove
/// model-only intent: visible compact summaries carry that flag as well.
fn model_only(frame: &Value) -> bool {
    frame["isMeta"].as_bool() == Some(true) && string(&frame["origin"], "kind") != "peer"
}

/// Preserve opaque provider context without attributing it to the user, a
/// particular tool, or another agent. System notices are explicit disclosures.
fn context_item(id: String, body: String) -> Item {
    notice_item(id, "Claude context", body)
}

/// Where compaction restarted the model's context, with the summary that
/// replaced the turns before it available on demand.
fn compaction_item(id: String, body: String) -> Item {
    notice_item(id, COMPACTED, body)
}

const COMPACTED: &str = "Context compacted";

fn notice_item(id: String, summary: &str, body: String) -> Item {
    Item {
        id,
        role: Role::System,
        content: ItemContent::Delivery {
            kind: DeliveryKind::Notice,
            from: String::new(),
            sender: String::new(),
            summary: summary.into(),
            body,
        },
        started_at: None,
        at: None,
    }
}

/// Why messages were skipped, in a sentence for each reason.
fn skipped_reasons(damaged: usize, oversized: usize) -> Vec<String> {
    let count = |count: usize| match count {
        1 => "One saved message".to_owned(),
        count => format!("{count} saved messages"),
    };
    let mut reasons = Vec::new();
    if damaged > 0 {
        reasons.push(format!(
            "{} could not be read: {} cut short, for example when Claude Code was stopped while saving.",
            count(damaged),
            if damaged == 1 { "it was" } else { "they were" }
        ));
    }
    if oversized > 0 {
        reasons.push(format!(
            "{} {} too large to show, such as a very long tool output.",
            count(oversized),
            if oversized == 1 { "is" } else { "are" }
        ));
    }
    reasons
}

/// What a transcript's skipped messages were, or `None` when none were
/// skipped. `marked` says each is marked where it was in the conversation.
fn skipped_records(
    damaged: usize,
    oversized: usize,
    marked: bool,
    resumes: bool,
) -> Option<String> {
    let mut lines = skipped_reasons(damaged, oversized);
    if lines.is_empty() {
        return None;
    }
    if marked {
        lines.push(
            if damaged + oversized == 1 {
                "It is marked where it was."
            } else {
                "Each is marked where it was."
            }
            .into(),
        );
    }
    lines.push(
        if resumes {
            "Everything else in the conversation is shown, and Claude Code continues it as usual."
        } else {
            "Everything else in the conversation is shown."
        }
        .into(),
    );
    Some(lines.join(" "))
}

/// Marks where messages that could not be shown were in a conversation.
fn gap_item(gap: Gap) -> Item {
    let Gap {
        at,
        damaged,
        oversized,
    } = gap;
    let total = damaged + oversized;
    let summary = match (damaged, oversized) {
        (1, 0) => "A message here could not be read".to_owned(),
        (_, 0) => format!("{damaged} messages here could not be read"),
        (0, 1) => "A message here is too large to show".to_owned(),
        (0, _) => format!("{oversized} messages here are too large to show"),
        _ => format!("{total} messages here are not shown"),
    };
    notice_item(
        format!("history-gap:{at}"),
        &summary,
        skipped_reasons(damaged, oversized).join(" "),
    )
}

/// A disclosure about the saved history itself, ahead of what it shows.
fn history_notice(summary: &str, body: String) -> Item {
    notice_item(format!("history-notice:{summary}"), summary, body)
}

fn unsupported_content_item(result: String) -> Item {
    Item {
        id: format!("{result}:unsupported-content"),
        role: Role::System,
        content: ItemContent::Delivery {
            kind: DeliveryKind::Notice,
            from: String::new(),
            sender: String::new(),
            summary: "Claude sent unsupported content, which was omitted".into(),
            body: String::new(),
        },
        started_at: None,
        at: None,
    }
}

/// The text of a message whose content is a string or a list of text blocks.
fn message_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.into());
    }
    let blocks = content.as_array()?;
    let texts: Vec<&str> = blocks
        .iter()
        .filter(|block| string(block, "type") == "text")
        .map(|block| string(block, "text"))
        .collect();
    (texts.len() == blocks.len() && !texts.is_empty()).then(|| texts.concat())
}

/// A peer delivery as parsed, before its harness framing is removed.
#[derive(Default)]
struct Peer {
    from: String,
    sender: String,
    body: String,
}

impl Peer {
    /// The delivery with the framing the harness adds for the model removed.
    ///
    /// A hand-back opens with a paragraph telling the model the report is not
    /// the user's, then indents every line of the report so a forged frame
    /// inside it cannot sit at column zero. Neither is meant for a reader.
    fn finish(mut self, handback: bool) -> ItemContent {
        // An unnamed subagent is addressed by its task ID alone.
        if self.from == self.sender {
            self.from.clear();
        }
        let (report, body) = match self.body.strip_prefix("[Subagent hand-back]") {
            Some(framed) => (
                true,
                framed.split_once('\n').map_or("", |(_, report)| report),
            ),
            None => (handback, self.body.as_str()),
        };
        let body = if report {
            body.lines()
                .map(|line| line.strip_prefix("  ").unwrap_or(line.trim_start()))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            body.to_owned()
        };
        // A teammate's task ID repeats its name; only an opaque one tells
        // two senders of the same name apart.
        let sender = if !self.sender.is_empty()
            && self.sender.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            self.sender
        } else {
            String::new()
        };
        ItemContent::Delivery {
            kind: if report {
                DeliveryKind::Report
            } else {
                DeliveryKind::Message
            },
            from: self.from,
            sender,
            summary: String::new(),
            body: body.trim().into(),
        }
    }
}

/// `content` after the `Another Claude session sent a message:` line the
/// harness introduces a peer's envelope with, where there is one.
fn without_preface(content: &str) -> &str {
    match content.split_once('\n') {
        Some((preface, rest))
            if preface.starts_with("Another Claude session sent a message")
                && preface.ends_with(':') =>
        {
            rest
        }
        _ => content,
    }
}

/// Splits an element whose opening tag is a line of its own into its
/// attributes, its body and whatever follows its closing tag, which must
/// also stand on a line of its own. `content` starts at the opening tag.
fn element<'a>(content: &'a str, tag: &str) -> Option<(&'a str, &'a str, &'a str)> {
    let (open, rest) = content.split_once('\n')?;
    let attributes = open
        .strip_prefix('<')?
        .strip_prefix(tag)?
        .strip_suffix('>')?;
    if !(attributes.is_empty() || attributes.starts_with(' ')) {
        return None;
    }
    let close = format!("</{tag}>");
    if let Some(after) = rest.strip_prefix(&close) {
        return Some((attributes, "", after));
    }
    let end = rest.find(&format!("\n{close}"))?;
    Some((attributes, &rest[..end], &rest[end + 1 + close.len()..]))
}

/// The value of attribute `name` among an element's `attributes`, with the
/// XML escapes the harness writes there undone.
fn attribute(attributes: &str, name: &str) -> Option<String> {
    let pattern = format!(" {name}=\"");
    let start = attributes.find(&pattern)? + pattern.len();
    let end = attributes[start..].find('"')?;
    Some(decode_xml_text(&attributes[start..start + end]))
}

/// Parses an `<agent-message>` or `<cross-session-message>` element, which is
/// how the harness presents a peer's message to the model. A queued
/// hand-back has no preface. Whatever follows the closing tag is the
/// harness's advice to the model and is dropped.
fn peer_envelope(content: &str) -> Option<Peer> {
    let content = without_preface(content);
    let (attributes, body) = ["agent-message", "cross-session-message"]
        .into_iter()
        .find_map(|tag| element(content, tag))
        .map(|(attributes, body, _)| (attributes, body))?;
    let from = attribute(attributes, "from").unwrap_or_default();
    Some(match attribute(attributes, "from-name") {
        Some(name) => Peer {
            from: name,
            sender: String::new(),
            body: body.into(),
        },
        None => Peer {
            from: from.clone(),
            sender: from,
            body: body.into(),
        },
    })
}

/// Parses the `<teammate-message>` elements an agent team's lead is sent,
/// one per message that arrived while it was busy.
///
/// A teammate going idle is announced the same way, as JSON. The final
/// message it carries is the teammate's report; an idle teammate with none
/// is only a notice.
fn teammate_messages(content: &str) -> Vec<ItemContent> {
    let mut rest = without_preface(content);
    let mut deliveries = Vec::new();
    while let Some((attributes, body, after)) = element(rest.trim_start(), "teammate-message") {
        rest = after;
        let from = attribute(attributes, "teammate_id").unwrap_or_default();
        let summary = attribute(attributes, "summary").unwrap_or_default();
        let event = serde_json::from_str::<Value>(body)
            .ok()
            .filter(|event| string(event, "type") == "idle_notification");
        let Some(event) = event else {
            deliveries.push(ItemContent::Delivery {
                kind: DeliveryKind::Message,
                from,
                sender: String::new(),
                summary,
                body: body.trim().into(),
            });
            continue;
        };
        let result = string(&event, "result").trim();
        let failure = string(&event, "failureReason").trim();
        // The event's own `summary` previews the last message the teammate
        // sent someone, not what it reports, so it is no heading.
        let summary = if failure.is_empty() {
            summary
        } else {
            format!("Stopped: {failure}")
        };
        deliveries.push(if result.is_empty() {
            ItemContent::Delivery {
                kind: DeliveryKind::Notice,
                summary: if summary.is_empty() {
                    format!("{from} is idle")
                } else {
                    format!("{from}: {summary}")
                },
                from,
                sender: String::new(),
                body: String::new(),
            }
        } else {
            ItemContent::Delivery {
                kind: DeliveryKind::Report,
                from,
                sender: String::new(),
                summary,
                body: result.into(),
            }
        });
    }
    deliveries
}

const NOTIFICATION_OPEN: &str = "<task-notification>";

/// What the harness answers for an agent that handed its report back as a
/// peer message, which the hand-back already showed.
const HANDED_BACK: &str = "This agent's report was delivered to you as a message";

/// What the harness answers for an agent that stopped before reporting.
const NOT_REPORTED: &str = "This agent has not reported yet";

/// What the harness answers for an agent that ended without handing a report
/// back, whose last words it withholds.
const UNSENT: &str = "The subagent ended without delivering a report";

/// What to show for a harness pointer in place of an agent's result: nothing
/// for a report already shown, and the pointer itself for one never made.
/// `None` when `result` is the report.
fn pointer(result: &str) -> Option<&str> {
    if result.starts_with(HANDED_BACK) {
        Some("")
    } else if result.starts_with(NOT_REPORTED) || result.starts_with(UNSENT) {
        Some(result)
    } else {
        None
    }
}

/// Parses the `<task-notification>` elements in `content`: the harness
/// telling the model a background command, monitor or agent finished or
/// produced an event. Text around them is the harness's advice to the model.
fn task_notifications(content: &str) -> Vec<ItemContent> {
    notification_elements(content)
        .into_iter()
        .map(task_notification)
        .collect()
}

/// The inner text of each `<task-notification>` element in `content`.
fn notification_elements(content: &str) -> Vec<&str> {
    const CLOSE: &str = "</task-notification>";
    let mut elements = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find(NOTIFICATION_OPEN) {
        let after = &rest[start + NOTIFICATION_OPEN.len()..];
        // A result or event quotes an agent or a command, either of which may
        // print the closing tag itself; the one the harness wrote is the last
        // before the next element.
        let next = after
            .find(&format!("\n{NOTIFICATION_OPEN}"))
            .unwrap_or(after.len());
        let Some(end) = after[..next].rfind(CLOSE) else {
            break;
        };
        elements.push(&after[..end]);
        rest = &after[next..];
    }
    elements
}

/// What a restored background command says when its session ended before
/// its notification was recorded.
const UNRECORDED_BACKGROUND_RESULT: &str =
    "Ran in the background until the session ended. Its result was not recorded.";

/// The calls a notification reports background work they started ended
/// for, each with how it ended. Only a notification that names its call and
/// says how it ended settles one: a Monitor's event names neither.
fn notified_calls(content: &str) -> Vec<(String, TaskOutcome)> {
    notified_call_tasks(content)
        .into_iter()
        .map(|(tool, _, outcome)| (tool, outcome))
        .collect()
}

fn notified_call_tasks(content: &str) -> Vec<(String, String, TaskOutcome)> {
    notification_heads(content)
        .into_iter()
        .filter_map(|head| {
            let tool = notification_field(head, "tool-use-id").filter(|tool| !tool.is_empty())?;
            let status = task_status(&notification_field(head, "status")?, false)?;
            let summary = notification_field(head, "summary").unwrap_or_default();
            Some((
                tool,
                notification_field(head, "task-id").unwrap_or_default(),
                TaskOutcome::new(status, &summary),
            ))
        })
        .collect()
}

/// The trimmed text of the `name` child of a task notification's `inner` text.
fn notification_field(inner: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = inner.find(&open)? + open.len();
    let end = if matches!(name, "result" | "event") {
        inner[start..].rfind(&close)?
    } else {
        inner[start..].find(&close)?
    };
    Some(decode_xml_text(inner[start..start + end].trim()))
}

/// Decode one XML text/attribute boundary. Splitting the original input once
/// prevents a literal `&amp;lt;` from becoming `<`; malformed references survive.
fn decode_xml_text(text: &str) -> String {
    let mut parts = text.split('&');
    let mut decoded = parts.next().unwrap_or_default().to_owned();
    for part in parts {
        let entity = part.split_once(';').and_then(|(name, rest)| {
            let value = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => name.strip_prefix('#').and_then(|number| {
                    let (digits, radix) = number
                        .strip_prefix('x')
                        .or_else(|| number.strip_prefix('X'))
                        .map_or((number, 10), |digits| (digits, 16));
                    (!digits.is_empty() && digits.chars().all(|c| c.is_digit(radix)))
                        .then(|| u32::from_str_radix(digits, radix).ok())
                        .flatten()
                        .and_then(char::from_u32)
                        .filter(|c| *c != '\0')
                }),
            }?;
            Some((value, rest))
        });
        match entity {
            Some((value, rest)) => {
                decoded.push(value);
                decoded.push_str(rest);
            }
            None => {
                decoded.push('&');
                decoded.push_str(part);
            }
        }
    }
    decoded
}

/// The tasks a notification's head names. A resumed session reports all its
/// orphaned agents in one element, with a `<task-id>` for each.
fn notification_tasks(head: &str) -> Vec<String> {
    let mut tasks = Vec::new();
    let mut rest = head;
    while let Some(task) = notification_field(rest, "task-id") {
        tasks.push(task);
        let Some((_, after)) = rest.split_once("</task-id>") else {
            break;
        };
        rest = after;
    }
    if tasks.is_empty() {
        tasks.push(String::new());
    }
    tasks
}

/// What a notification says its task is doing now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskNotice {
    /// It has ended, the same as if a system frame had said so.
    Ended(ToolStatus),
    /// It stopped with background work of its own still running, so it is alive
    /// and may report again, though it isn't certain to.
    Waiting,
    /// Another agent took it over; its completion won't be delivered here.
    Elsewhere,
    /// A check-in with no status: it's running.
    Running,
}

const WAITING_ON_BACKGROUND: &str =
    "Waiting on its own background work; the result so far may be interim";
const ENDED_UNREPORTED: &str = "Stopped running without reporting how it went";
const REPORTS_ELSEWHERE: &str = "Continued by another agent, which gets its result";
const WAITING_SUMMARY: &str = "is waiting on its own background work";
/// How Claude Code 2.1.296 words the same pause, for an agent that can hand
/// its report back later: its own pointer, in place of any description.
const WAITING_POINTER: &str =
    "This agent has not reported yet: it is waiting on its own background work";
const INTERIM_NOTE: &str = "may be interim";
const ELSEWHERE_SUMMARY: &str = "completion will not be delivered here";

/// Whether a `completed` report is only the agent pausing. Claude Code says so
/// at the end of the summary, after the agent's own description, at the start
/// of the summary when that is its own pointer, or on older versions only in
/// the note beside it.
fn interim_notice(status: &str, fields: &Value) -> bool {
    let summary = string(fields, "summary");
    status == "completed"
        && (summary
            .trim_end_matches(['.', ' ', '\n'])
            .ends_with(WAITING_SUMMARY)
            || summary.starts_with(WAITING_POINTER)
            || string(fields, "note").contains(INTERIM_NOTE))
}

/// The same notice's head as the fields `interim_notice` and friends read.
fn notice_fields(head: &str) -> Value {
    json!({
        "summary": notification_field(head, "summary").unwrap_or_default(),
        "note": notification_field(head, "note").unwrap_or_default(),
    })
}

/// What the recorded task notifications in `content` say of their tasks, as
/// each one's tool-use ID, task ID and status, `Waiting` for one still going. Claude Code records a
/// background agent finishing only this way: the streamed `task_notification`
/// frame never reaches its transcript.
fn notification_outcomes(content: &str) -> Vec<(String, String, ToolStatus)> {
    // The same heads the live notice is read from, so a notice quoted in a
    // result settles nothing here either.
    let mut heads = notification_heads(content);
    // A result quoting a notification can hide the element's end; the first
    // element's identity and status precede its result all the same.
    if heads.is_empty()
        && let Some(start) = content.find(NOTIFICATION_OPEN)
    {
        let inner = &content[start + NOTIFICATION_OPEN.len()..];
        let end = ["<result>", "<event>", "</task-notification>"]
            .into_iter()
            .filter_map(|tag| inner.find(tag))
            .min()
            .unwrap_or(inner.len());
        heads.push(&inner[..end]);
    }
    heads
        .into_iter()
        .flat_map(|head| {
            let tool = notification_field(head, "tool-use-id").unwrap_or_default();
            let status = notification_field(head, "status").unwrap_or_default();
            let fields = notice_fields(head);
            let outcome = if interim_notice(&status, &fields) {
                Some(ToolStatus::Waiting)
            } else if !status.is_empty() {
                task_status(&status, false)
            } else if string(&fields, "summary").contains(ELSEWHERE_SUMMARY) {
                Some(ToolStatus::Stopped)
            } else {
                // A check-in without a status, such as a resume, says the agent
                // was still going, as it does live. History reads it like a
                // pause: the agent was alive at its last word, and ends as one
                // left waiting does unless a later notice settles it.
                Some(ToolStatus::Waiting)
            };
            notification_tasks(head)
                .into_iter()
                .filter_map(move |task| Some((tool.clone(), task, outcome?)))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn bounded_failure_detail<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut detail = String::new();
    for part in parts {
        let part = part.trim();
        if part.is_empty() || detail == part {
            continue;
        }
        if !detail.is_empty() {
            detail.push_str("\n\n");
        }
        let remaining = RECORDED_SUMMARY_BYTES.saturating_sub(detail.len());
        detail.push_str(&part[..part.floor_char_boundary(remaining.min(part.len()))]);
        if detail.len() >= RECORDED_SUMMARY_BYTES {
            break;
        }
    }
    if detail.is_empty() {
        "The agent failed without reporting a reason".into()
    } else {
        detail
    }
}

fn notification_failure_detail(element: &str) -> String {
    let summary = notification_field(element, "summary").unwrap_or_default();
    let result = notification_field(element, "result").unwrap_or_default();
    bounded_failure_detail([summary.as_str(), result.as_str()])
}

/// What the tasks in a live notification are doing, as tool-use ID, task ID and
/// notice. Only the head of the element says so: Claude Code also sends
/// statusless check-ins for work that is still running, and a Monitor's
/// `<event>` may carry text of its own.
fn live_notifications(content: &str) -> Vec<(String, String, TaskNotice)> {
    let mut outcomes = Vec::new();
    for head in notification_heads(content) {
        let status = notification_field(head, "status").unwrap_or_default();
        let fields = notice_fields(head);
        let notice = if interim_notice(&status, &fields) {
            TaskNotice::Waiting
        } else if let Some(ended) = task_status(&status, false) {
            TaskNotice::Ended(ended)
        } else if !status.is_empty() {
            continue;
        } else if string(&fields, "summary").contains(ELSEWHERE_SUMMARY) {
            TaskNotice::Elsewhere
        } else {
            TaskNotice::Running
        };
        let tool = notification_field(head, "tool-use-id").unwrap_or_default();
        for task in notification_tasks(head) {
            outcomes.push((tool.clone(), task, notice));
        }
    }
    outcomes
}

/// The head of each `<task-notification>` element in `content`: what precedes
/// its result or event. A result is opaque and may quote a notification, so
/// an element counts only once its result has closed and the harness's close
/// follows; a truncated report says nothing.
fn notification_heads(content: &str) -> Vec<&str> {
    // Well beyond any real report, which current versions escape anyway. Each
    // candidate end rescans what follows it, on the provider's reader thread.
    const MAX_RESULT_ENDS: usize = 64;
    const CLOSE: &str = "</task-notification>";
    const RESULT_OPEN: &str = "<result>";
    const RESULT_END: &str = "</result>";
    // The length of a run of complete harness elements such as `<usage>` and
    // `<worktree>` through the close that follows them. Prose or a nested
    // notification means this wasn't the end of the element.
    let closed = |tail: &str| -> Option<usize> {
        // Harness elements never hold a close, so nothing past the first one
        // needs reading.
        let tail = &tail[..tail.find(CLOSE)? + CLOSE.len()];
        let mut at = tail;
        loop {
            at = at.trim_start();
            if let Some(after) = at.strip_prefix(CLOSE) {
                return Some(tail.len() - after.len());
            }
            let (name, _) = at.strip_prefix('<')?.split_once('>')?;
            if name.is_empty()
                || name == "task-notification"
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                return None;
            }
            at = at.split_once(&format!("</{name}>"))?.1;
        }
    };
    let mut heads = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find(NOTIFICATION_OPEN) {
        let inner = &rest[start + NOTIFICATION_OPEN.len()..];
        // The head ends before the first close, so the other tags are only
        // looked for up to there rather than through every later element.
        let close = inner.find(CLOSE).unwrap_or(inner.len());
        let head_end = [RESULT_OPEN, "<event>"]
            .into_iter()
            .filter_map(|tag| inner[..close].find(tag))
            .min()
            .unwrap_or(close);
        let head = &inner[..head_end];
        let body = &inner[head_end..];
        let next = if body.starts_with(RESULT_OPEN) {
            // Prefer the end that balances the result's tags and that another
            // element follows; otherwise any end a close follows.
            let (mut balanced, mut boundary, mut last) = (None, None, None);
            let (mut opens, mut ends, mut counted) = (0, 0, 0);
            let candidates = body
                .match_indices(RESULT_END)
                .map(|(at, _)| at)
                .take(MAX_RESULT_ENDS)
                // Only when no nearer end closed the element.
                .chain(std::iter::once_with(|| body.rfind(RESULT_END)).flatten());
            for at in candidates {
                if at < counted {
                    continue;
                }
                opens += body[counted..at].matches(RESULT_OPEN).count();
                ends += 1;
                counted = at + RESULT_END.len();
                let tail = &body[counted..];
                let Some(end) = closed(tail) else {
                    continue;
                };
                let after = &tail[end..];
                last = Some(after);
                if opens == ends {
                    balanced = Some(after);
                    if after.trim_start().starts_with(NOTIFICATION_OPEN) {
                        boundary = balanced;
                        break;
                    }
                }
            }
            boundary.or(balanced).or(last)
        } else if body.starts_with(CLOSE) {
            Some(&body[CLOSE.len()..])
        } else if body.starts_with("<event>") {
            body.split_once("</event>")
                .and_then(|(_, tail)| closed(tail).map(|end| &tail[end..]))
        } else {
            None
        };
        let Some(next) = next else {
            return heads;
        };
        heads.push(head);
        if !next.trim_start().starts_with(NOTIFICATION_OPEN) {
            break;
        }
        rest = next;
    }
    heads
}

/// A background agent's status change, with nothing else about it changing.
fn subagent_status(
    id: String,
    status: ToolStatus,
    detail: Option<&str>,
    background: bool,
) -> Event {
    Event::Subagent {
        model: None,
        waiting: None,
        detail: detail.map(str::to_owned),
        id,
        kind: String::new(),
        description: String::new(),
        status: Some(status),
        background: background.then_some(true),
    }
}

/// What a Claude task status says of the task, or `None` while it runs. A
/// notification is sent when a task stops, so one without a status reports
/// that it finished.
fn task_status(status: &str, notification: bool) -> Option<ToolStatus> {
    match status {
        "completed" => Some(ToolStatus::Succeeded),
        "failed" => Some(ToolStatus::Failed),
        "killed" | "stopped" | "cancelled" | "canceled" => Some(ToolStatus::Stopped),
        _ if notification => Some(ToolStatus::Succeeded),
        _ => None,
    }
}

/// The agent ID Claude Code gave the agent an `Agent` call started, which
/// `SendMessage` and `TaskStop` name it by: the structured result's, or the
/// `agentId: ` line its text carries for the model.
fn agent_address(structured: &Value, output: &str) -> Option<String> {
    structured["agentId"]
        .as_str()
        .or_else(|| {
            output
                .lines()
                .find_map(|line| line.strip_prefix("agentId: "))
                .and_then(|rest| rest.split_whitespace().next())
        })
        .filter(|address| !address.is_empty())
        .map(str::to_owned)
}

/// What Claude Code reports about a tool's result beside its text:
/// `tool_use_result` on the wire and `toolUseResult` in a transcript.
fn structured_result(frame: &Value) -> &Value {
    match &frame["tool_use_result"] {
        Value::Null => &frame["toolUseResult"],
        result => result,
    }
}

/// Whether a user frame carrying a single tool result acknowledges an agent
/// Claude Code runs in the background, which it may do unasked. The structured
/// result is `tool_use_result` on the wire and `toolUseResult` in a transcript.
fn async_launch(frame: &Value) -> bool {
    frame["message"]["content"]
        .as_array()
        .is_some_and(|blocks| {
            blocks
                .iter()
                .filter(|block| string(block, "type") == "tool_result")
                .count()
                == 1
        })
        && ["tool_use_result", "toolUseResult"]
            .into_iter()
            .any(|key| string(&frame[key], "status") == "async_launched")
}

/// The task notification text a transcript record holds, if any. Claude Code
/// records one as the queued command that delivered it to the model, as the
/// queue operation that held it until then, or, in older transcripts, as a
/// user frame.
fn recorded_notification(record: &Value) -> Option<String> {
    let enveloped = |text: String| {
        text.trim_start()
            .starts_with(NOTIFICATION_OPEN)
            .then_some(text)
    };
    match string(record, "type") {
        "attachment" => {
            let attachment = &record["attachment"];
            let notification = string(attachment, "commandMode") == "task-notification"
                || string(&attachment["origin"], "kind") == "task-notification";
            (string(attachment, "type") == "queued_command" && notification)
                .then(|| message_text(&attachment["prompt"]))
                .flatten()
        }
        "queue-operation" if string(record, "operation") == "enqueue" => record["content"]
            .as_str()
            .filter(|text| text.trim_start().starts_with(NOTIFICATION_OPEN))
            .map(str::to_owned),
        "user" => {
            let content = message_text(&record["message"]["content"]);
            match string(&record["origin"], "kind") {
                "task-notification" => content,
                "" => content.and_then(enveloped),
                _ => None,
            }
        }
        _ => None,
    }
}

fn task_notification(inner: &str) -> ItemContent {
    let element = |name: &str| notification_field(inner, name);
    let mut summary = element("summary")
        .filter(|summary| !summary.trim().is_empty())
        .unwrap_or_else(|| background_task_heading(&element("status").unwrap_or_default(), ""));
    // A finished agent's result is its report: headless Claude Code hands a
    // background agent's final message back this way rather than as a peer's.
    let (kind, mut body) = match element("result") {
        Some(result) => match pointer(&result) {
            Some(shown) => (DeliveryKind::Notice, shown.to_owned()),
            None => (DeliveryKind::Report, result),
        },
        None => (DeliveryKind::Notice, element("event").unwrap_or_default()),
    };
    if kind == DeliveryKind::Notice
        && let Some((heading, command)) = command_notification(
            &summary,
            &element("status").unwrap_or_default(),
            "",
            "",
            false,
        ) {
            summary = heading;
            body = command;
        }
    ItemContent::Delivery {
        kind,
        from: String::new(),
        sender: element("task-id").unwrap_or_default(),
        summary,
        body,
    }
}

/// A background completion's prose is a status, not a useful title or output.
/// Keep its command available once as literal detail and use only the original
/// Bash description as a human title. A lifecycle task's description can be the
/// raw shell command, so it is not sufficient provenance for a title.
fn command_notification(
    summary: &str,
    status: &str,
    description: &str,
    command: &str,
    known_command: bool,
) -> Option<(String, String)> {
    let rest = summary.strip_prefix("Background command ");
    if rest.is_none() && !known_command {
        return None;
    }
    let exit = summary_exit_code(summary);
    let outcome_text = background_command_outcome(summary);
    let failed = exit.is_some_and(|exit| exit != 0)
        || matches!(status, "failed" | "error")
        || outcome_text.is_some_and(|outcome| {
            outcome == "failed" || outcome.starts_with("failed with exit code ")
        });
    let outcome = if failed {
        exit.map_or_else(|| "failed".into(), |exit| format!("failed (exit {exit})"))
    } else if matches!(status, "stopped" | "killed")
        || outcome_text.is_some_and(|outcome| outcome.starts_with("was "))
    {
        "stopped".into()
    } else {
        "finished".into()
    };
    let heading = if description.trim().is_empty()
        || description.trim() == command.trim()
        || description.contains('\n')
    {
        format!("Background command {outcome}")
    } else {
        format!("{} · {outcome}", description.trim())
    };
    let detail = if command.trim().is_empty() {
        rest.zip(outcome_text)
            .and_then(|(text, outcome)| text.strip_suffix(outcome))
            .and_then(|subject| subject.trim_end().strip_prefix('"')?.strip_suffix('"'))
            .unwrap_or_default()
            .to_owned()
    } else {
        command.to_owned()
    };
    Some((heading, detail))
}

/// Use the same fallback when either transport omits an informative summary.
fn background_task_heading(status: &str, description: &str) -> String {
    let status = if status.trim().is_empty() {
        "updated"
    } else {
        status.trim()
    };
    if description.is_empty() {
        format!("Background task {status}")
    } else {
        format!("Background task \"{description}\" {status}")
    }
}

/// A streamed `task_notification` frame as the delivery its recorded turn
/// reads as. The frame carries an agent's result, or a command's account of
/// itself, as its `summary`; the agent's name is the description its task
/// started with.
fn live_notification(frame: &Value, description: &str, kind: Option<ReportTask>) -> ItemContent {
    let summary = string(frame, "summary").trim();
    let status = string(frame, "status");
    let task = string(frame, "task_id").to_owned();
    let Some(task_kind) = kind else {
        // Stream-json fields are JSON strings, not XML text. Their literal
        // entities must survive; only the recorded wrapper is decoded.
        let command = command_notification(summary, status, "", "", false);
        return ItemContent::Delivery {
            kind: DeliveryKind::Notice,
            from: String::new(),
            sender: task,
            summary: command
                .as_ref()
                .map(|(heading, _)| heading.clone())
                .unwrap_or_else(|| {
                    if summary.is_empty() {
                        background_task_heading(status, description)
                    } else {
                        summary.into()
                    }
                }),
            body: command
                .map(|(_, detail)| detail)
                .unwrap_or_else(|| string(frame, "event").to_owned()),
        };
    };
    let origin = match task_kind {
        ReportTask::Agent => "Agent",
        ReportTask::Workflow => "Workflow",
    };
    let subject = if description.is_empty() {
        origin.to_owned()
    } else {
        format!("{origin} \"{description}\"")
    };
    let outcome = match status {
        "failed" => "failed",
        "stopped" | "killed" => "was stopped",
        _ => "finished",
    };
    let heading = format!("{subject} {outcome}");
    let (kind, body) = match pointer(summary) {
        _ if summary.is_empty() => (DeliveryKind::Notice, ""),
        Some(shown) => (DeliveryKind::Notice, shown),
        None => (DeliveryKind::Report, summary),
    };
    ItemContent::Delivery {
        kind,
        from: String::new(),
        sender: task,
        summary: heading,
        body: body.into(),
    }
}

fn text_item(id: String, role: Role, text: String, thinking: bool) -> Item {
    Item {
        id,
        role,
        // Claude's stream-json frames carry no turn-phase label: an assistant
        // text block is shaped identically whether it narrates progress or
        // answers the question, and the closing `result` frame only repeats
        // text that was already delivered as its own block. A guessed phase
        // would demote real answers into narration, so every text item is
        // final until the provider itself says otherwise.
        content: ItemContent::Text {
            text,
            thinking,
            phase: Phase::Final,
        },
        started_at: None,
        at: None,
    }
}
/// Map a Claude tool call onto the transcript's richer variants.
///
/// Only Claude's own built-in tools are classified, because the tool's name is
/// what makes the intent unambiguous; an arbitrary tool's arguments are not
/// evidence of what it does. MCP tools and subagent tasks stay a tool
/// disclosure, and so does a built-in whose defining input has not arrived
/// yet, since a command item without its command tells the view less than the
/// tool call it came from.
fn tool_content(name: &str, input: &Value, cwd: &Path) -> ItemContent {
    if let Some(changes) = super::claude_edits::changes(name, input) {
        return ItemContent::Changes {
            changes,
            status: ToolStatus::Running,
        };
    }
    let session = || cwd.to_string_lossy().into_owned();
    // Claude's search tools walk `path` and fall back to the session directory
    // when it is absent, which is what the provider itself does.
    let scope = || match input["path"].as_str().filter(|path| !path.is_empty()) {
        Some(path) => path.to_owned(),
        None => session(),
    };
    let pattern = string(input, "pattern");
    let file = string(input, "file_path");
    match name {
        "Bash" if !string(input, "command").is_empty() => command_content(
            // Claude writes its own short summary of the command it is running.
            string(input, "description").into(),
            string(input, "command").into(),
            session(),
            CommandAction::Run,
        ),
        // What a read, a search or a listing acted on is its label; the view
        // supplies the verb, from the action.
        "Read" if !file.is_empty() => {
            command_content(file.into(), file.into(), session(), CommandAction::Read)
        }
        "Grep" if !pattern.is_empty() => command_content(
            pattern.into(),
            pattern.into(),
            scope(),
            CommandAction::Search,
        ),
        "Glob" if !pattern.is_empty() => {
            command_content(pattern.into(), pattern.into(), scope(), CommandAction::List)
        }
        _ => ItemContent::Tool {
            name: name.into(),
            input: input.to_string(),
            output: String::new(),
            status: ToolStatus::Running,
        },
    }
}
/// A streaming tool call as the view is shown it while its input arrives.
///
/// How a call is presented depends on its whole input, which only exists once
/// its block stops. A row that read "Called Bash" while a command streamed and
/// "Ran cargo test" once it stopped would change what it says it did, so a call
/// whose name settles its verb takes that form from its first fragment, filled
/// with whatever of its input has arrived. An edit's verb depends on what it
/// replaces, so it is an edit not yet described; any other call is shown as
/// the tool call it stays.
fn streamed(item: &Item, input: &StreamedInput, cwd: &Path) -> Item {
    let ItemContent::Tool { name, .. } = &item.content else {
        return item.clone();
    };
    // Only the presentation is new: the call's raw input, which a command
    // row does not carry, is not copied for every fragment.
    let shown = |content| Item {
        started_at: item.started_at,
        id: item.id.clone(),
        role: item.role,
        content,
        at: item.at,
    };
    let action = match name.as_str() {
        "Bash" => CommandAction::Run,
        "Read" => CommandAction::Read,
        "Grep" => CommandAction::Search,
        "Glob" => CommandAction::List,
        "Edit" | "MultiEdit" | "Write" => {
            return shown(ItemContent::Changes {
                changes: Vec::new(),
                status: ToolStatus::Running,
            });
        }
        _ => return item.clone(),
    };
    let content = match tool_content(name, input.value(), cwd) {
        content @ ItemContent::Command { .. } => content,
        _ => command_content(
            String::new(),
            String::new(),
            cwd.to_string_lossy().into_owned(),
            action,
        ),
    };
    shown(content)
}

fn command_content(
    label: String,
    command: String,
    working_directory: String,
    action: CommandAction,
) -> ItemContent {
    ItemContent::Command {
        label,
        command,
        working_directory,
        action,
        status: ToolStatus::Running,
        // Claude reports no duration for its tools, and an exit status only
        // for a command that failed or ran in the background; an inferred one
        // would be indistinguishable in the view from a measured one.
        exit_code: None,
        duration_ms: None,
        output: String::new(),
    }
}
/// A history page's items, each once: where it first appeared, as it last
/// stood. Replaying a tool reports its call and then its result as the same
/// item, and a page keeps an item's first copy.
fn settled(items: Vec<Item>) -> Vec<Item> {
    let mut positions = HashMap::new();
    let mut page: Vec<Item> = Vec::with_capacity(items.len());
    for item in items {
        if let Some(&position) = positions.get(&item.id) {
            page[position] = item;
        } else {
            positions.insert(item.id.clone(), page.len());
            page.push(item);
        }
    }
    page
}
/// Attach a tool result to whichever variant the call was mapped to, so that a
/// richer mapping never costs the result the provider reported.
///
/// `edit` is the tool an edit was made with, `input` the call's own input
/// where it is still held, and `structured` what Claude Code reported beside
/// the result's text.
fn apply_result(
    content: &mut ItemContent,
    text: String,
    failed: bool,
    edit: Option<&str>,
    input: Option<&str>,
    structured: &Value,
) {
    let result = match (failed, refusal(&text, structured)) {
        (false, _) => ToolStatus::Succeeded,
        (true, Some(status)) => status,
        (true, None) => ToolStatus::Failed,
    };
    match content {
        ItemContent::Changes { changes, status } if !failed => {
            super::claude_edits::applied(changes, structured);
            *status = result;
        }
        // An edit that failed changed nothing, and why it failed is the one
        // thing a reader needs: a patch has nowhere to say it, so the step
        // goes back to being the tool call it was, carrying the message.
        ItemContent::Changes { changes, .. } => {
            let input = input.map(str::to_owned).unwrap_or_else(|| {
                json!({"file_path": changes.first().map(|change| change.path.as_str())}).to_string()
            });
            *content = ItemContent::Tool {
                name: edit.unwrap_or("Edit").into(),
                input,
                output: text,
                status: result,
            };
        }
        // Only a shell command's result is what a terminal was written to: a
        // file that was read is shown as the bytes it holds.
        ItemContent::Command {
            output,
            status,
            exit_code,
            action: CommandAction::Run,
            ..
        } => {
            // Claude Code puts a failed command's exit status at the head of
            // its output, which is the only place it reports one.
            if failed {
                *exit_code = text
                    .strip_prefix("Exit code ")
                    .and_then(|rest| rest.split(['\n', ' ']).next())
                    .and_then(|code| code.parse().ok());
            }
            *output = super::terminal_output::display(match result {
                // The tags around an abort are Claude Code's framing for the
                // model; the reader is shown the sentence.
                ToolStatus::Stopped => text
                    .trim()
                    .strip_prefix("<error>")
                    .and_then(|inner| inner.strip_suffix("</error>"))
                    .map(str::to_owned)
                    .unwrap_or(text),
                _ => text,
            });
            *status = result;
        }
        ItemContent::Command { output, status, .. } | ItemContent::Tool { output, status, .. } => {
            *output = text;
            *status = result;
        }
        // `tool_content` maps a Claude tool call to `Command`, `Changes` or
        // `Tool` and to nothing else, so the remaining variants never hold a
        // Claude tool result.
        ItemContent::Text { .. }
        | ItemContent::WebSearch { .. }
        | ItemContent::Image { .. }
        | ItemContent::Delivery { .. } => {}
    }
}
/// Whether a failed result is the call being refused or cut short rather
/// than failing: the person declined it, or it was interrupted before it
/// finished. Claude Code reports both as errors, in fixed wording.
fn refusal(text: &str, structured: &Value) -> Option<ToolStatus> {
    let text = text.trim_start();
    if text.starts_with("The user doesn't want to proceed with this tool use") {
        Some(ToolStatus::Denied)
    } else if structured["interrupted"].as_bool() == Some(true)
        || text.starts_with("<error>Command was aborted before completion")
        || text.starts_with("[Request interrupted by user")
        || text.starts_with("[Tool call interrupted")
    {
        Some(ToolStatus::Stopped)
    } else {
        None
    }
}

/// The harness's complete terminal outcome, excluding its quoted subject.
fn background_command_outcome(summary: &str) -> Option<&str> {
    let rest = summary.strip_prefix("Background command ")?.trim();
    let terminal = |outcome: &str| {
        matches!(
            outcome,
            "completed"
                | "failed"
                | "was stopped"
                | "was stopped because the system is running low on memory"
                | "was stopped after reaching its background time limit"
        ) || background_outcome_exit_code(outcome).is_some()
    };
    let Some(quoted) = rest.strip_prefix('"') else {
        return terminal(rest).then_some(rest);
    };
    quoted.rmatch_indices('"').find_map(|(at, _)| {
        let outcome = quoted[at + 1..].trim();
        terminal(outcome).then_some(outcome)
    })
}

/// Parse the whole exit clause so a quoted phrase in an explanation cannot
/// become the terminal outcome. Parentheses inside quotes are literal output.
fn background_outcome_exit_code(outcome: &str) -> Option<i32> {
    if let Some(code) = outcome.strip_prefix("failed with exit code ") {
        return code.parse().ok();
    }
    let clause = outcome
        .strip_prefix("completed (exit code ")
        .or_else(|| outcome.strip_prefix("failed (exit code "))?
        .strip_suffix(')')?;
    let (code, explanation) = clause.split_once(": ").unwrap_or((clause, ""));
    let code = code.parse().ok()?;
    let (mut depth, mut quoted, mut escaped) = (0usize, false, false);
    for character in explanation.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth = depth.checked_sub(1)?,
            _ => {}
        }
    }
    (depth == 0 && !quoted && !escaped).then_some(code)
}

/// The exit status in the outcome, never a phrase inside the command itself.
fn summary_exit_code(summary: &str) -> Option<i32> {
    background_outcome_exit_code(background_command_outcome(summary)?)
}

fn image_content(block: &Value) -> ItemContent {
    let source = &block["source"];
    let path = match string(source, "type") {
        "base64"
            if matches!(
                string(source, "media_type"),
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            ) =>
        {
            format!(
                "data:{};base64,{}",
                string(source, "media_type"),
                string(source, "data")
            )
        }
        "url" => string(source, "url").to_owned(),
        _ => String::new(),
    };
    ItemContent::Image {
        reference: None,
        path,
        description: "Claude image attachment".into(),
    }
}
fn content_text(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        s.into()
    } else if let Some(blocks) = value.as_array() {
        blocks
            .iter()
            .map(|b| {
                if matches!(string(b, "type"), "image" | "document") {
                    format!("[{} attachment]", string(b, "type"))
                } else {
                    b.as_str()
                        .or_else(|| b["text"].as_str())
                        .map(str::to_owned)
                        .unwrap_or_else(|| b.to_string())
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else if value.is_null() {
        String::new()
    } else {
        value.to_string()
    }
}
// Question and option strings are provider-authored text, not attachment
// descriptors. Preserve them verbatim: they also serve as answer-map keys.
// A filename or shell pattern in a question does not authorize a file read.
fn question_fields(input: &Value) -> Result<Vec<Field>, String> {
    let questions = input["questions"]
        .as_array()
        .ok_or("Claude AskUserQuestion has no questions array")?;
    if questions.is_empty() || questions.len() > 4 {
        return Err("Claude AskUserQuestion has an invalid question count".into());
    }
    let mut fields = Vec::<Field>::new();
    for question in questions {
        let text = required(question, "question")?;
        if fields.iter().any(|f| f.id == text) {
            return Err("Claude AskUserQuestion has duplicate question keys".into());
        }
        let choices = question["options"]
            .as_array()
            .ok_or("Claude question is missing options")?
            .iter()
            .map(|o| {
                Ok(Choice {
                    id: required(o, "label")?.into(),
                    label: required(o, "label")?.into(),
                    description: string(o, "description").into(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        if !(2..=4).contains(&choices.len()) {
            return Err("Claude question must have two to four options".into());
        }
        fields.push(Field {
            id: text.into(),
            label: text.into(),
            description: string(question, "header").into(),
            choices,
            multiple: question["multiSelect"].as_bool().unwrap_or(false),
            secret: false,
            initial: String::new(),
        });
    }
    Ok(fields)
}
fn attachment_block(cwd: &Path, path: &Path) -> Result<Value, String> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .map_err(|e| format!("Cannot open attachment {}: {e}", path.display()))?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_DOCUMENT {
        return Err(
            "Claude attachments must be regular files: images up to 5 MiB, documents up to 20 MiB"
                .into(),
        );
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_DOCUMENT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_DOCUMENT {
        return Err("Claude attachment grew beyond the 20 MiB document limit".into());
    }
    let media = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    };
    if let Some(media) = media {
        if bytes.len() as u64 > MAX_IMAGE {
            return Err("Claude images must be no larger than 5 MiB".into());
        }
        Ok(
            json!({"type":"image", "source":{"type":"base64", "media_type":media, "data":base64::engine::general_purpose::STANDARD.encode(bytes)}}),
        )
    } else if bytes.starts_with(b"%PDF-") {
        Ok(
            json!({"type":"document", "source":{"type":"base64", "media_type":"application/pdf", "data":base64::engine::general_purpose::STANDARD.encode(bytes)}}),
        )
    } else {
        let text = String::from_utf8(bytes).map_err(|_| {
            "Unsupported Claude attachment; use PNG, JPEG, GIF, WebP, PDF, or UTF-8 text".to_owned()
        })?;
        Ok(
            json!({"type":"document", "title":path.file_name().unwrap_or_default().to_string_lossy(), "source":{"type":"text", "media_type":"text/plain", "data":text}}),
        )
    }
}

/// Count encoded bytes without allocating another copy of a large attachment.
fn encoded_len(value: &impl serde::Serialize) -> Result<usize, String> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 >= MAX_PAYLOAD {
                return Err(std::io::Error::other(
                    "Claude serialized payload must be smaller than 30 MiB",
                ));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).map_err(|e| e.to_string())?;
    Ok(counter.0)
}

impl Claude {
    fn stream(&mut self, frame: &Value, step: &mut Step) -> Result<(), String> {
        let event = &frame["event"];
        let parent = string(frame, "parent_tool_use_id").to_owned();
        match required(event, "type")? {
            "message_start" => {
                if self.streams.len() >= MAX_PENDING && !self.streams.contains_key(&parent) {
                    return Err("Too many simultaneous Claude streams".into());
                }
                let id = required(&event["message"], "id")?.to_owned();
                if self
                    .streams
                    .get(&parent)
                    .is_none_or(|stream| stream.message != id)
                {
                    let stream = Stream {
                        message: id,
                        truncated: false,
                    };
                    // A message interrupted before its `message_stop` is
                    // abandoned once its stream moves on.
                    if let Some(abandoned) = self.streams.insert(parent.clone(), stream) {
                        self.settle(Some(&parent), &abandoned.message);
                    }
                }
                if parent.is_empty() {
                    self.start_turn(step);
                    self.update_usage(&event["message"]["usage"], step);
                    self.status(true, "Claude is working", step);
                } else {
                    // Only actual new child API activity clears a retryable
                    // child failure; an unrelated background notice cannot.
                    step.events.push(Event::Subagent {
                        id: parent.clone(),
                        kind: String::new(),
                        description: String::new(),
                        status: self
                            .unconfirmed_agents
                            .remove(&parent)
                            .then_some(ToolStatus::Running),
                        background: None,
                        model: None,
                        waiting: Some(false),
                        detail: Some(String::new()),
                    });
                }
            }
            "content_block_start" => {
                let block = &event["content_block"];
                let block_type = required(block, "type")?;
                // Overflow still requires valid known protocol fields.
                if matches!(block_type, "tool_use" | "server_tool_use") {
                    required(block, "id")?;
                    required(block, "name")?;
                }
                let stream = self
                    .streams
                    .get_mut(&parent)
                    .ok_or("Claude block arrived before message_start")?;
                let message = stream.message.clone();
                let index = event["index"].as_u64().ok_or("Claude block has no index")?;
                let key = (ScopedId::new(Some(&parent), &message), index);
                self.blocks.remove(&key);
                self.ignored_blocks.remove(&key);
                if self.blocks.len() + self.ignored_blocks.len() >= MAX_TOOLS {
                    if !stream.truncated {
                        stream.truncated = true;
                        step.events.push(Event::Error {
                            message: "Claude message has too many streaming blocks; its remaining blocks appear as each completes".into(),
                            fatal: false,
                        });
                    }
                    return Ok(());
                }
                let item = match block_type {
                    "text" => text_item(
                        format!("{message}:{index}"),
                        Role::Assistant,
                        string(block, "text").into(),
                        false,
                    ),
                    "thinking" => text_item(
                        format!("{message}:{index}"),
                        Role::Assistant,
                        string(block, "thinking").into(),
                        true,
                    ),
                    "redacted_thinking" => text_item(
                        format!("{message}:{index}"),
                        Role::Assistant,
                        "[Redacted thinking]".into(),
                        true,
                    ),
                    "tool_use" | "server_tool_use" => Item {
                        id: required(block, "id")?.into(),
                        role: Role::Assistant,
                        content: ItemContent::Tool {
                            name: required(block, "name")?.into(),
                            input: String::new(),
                            output: String::new(),
                            status: ToolStatus::Running,
                        },
                        started_at: None,
                        at: None,
                    },
                    other => {
                        self.ignored_blocks.insert(key);
                        self.warn_unknown("stream content", other);
                        if parent.is_empty() {
                            self.turn_unsupported_content = true;
                        }
                        return Ok(());
                    }
                };
                if parent.is_empty() {
                    self.turn_visible_content = true;
                }
                let block = Block {
                    item,
                    input: StreamedInput::default(),
                };
                if !matches!(&block.item.content, ItemContent::Tool { .. })
                    || !self.completed_tool(Some(&parent), &block.item.id)
                {
                    step.events
                        .push(Event::Item(streamed(&block.item, &block.input, &self.cwd)));
                }
                self.blocks
                    .insert((ScopedId::new(Some(&parent), &message), index), block);
            }
            "content_block_delta" => {
                let stream = self
                    .streams
                    .get(&parent)
                    .ok_or("Claude delta arrived before message_start")?;
                let index = event["index"].as_u64().ok_or("Claude delta has no index")?;
                let delta = &event["delta"];
                let delta_type = required(delta, "type")?;
                if self
                    .ignored_blocks
                    .contains(&(ScopedId::new(Some(&parent), &stream.message), index))
                {
                    return Ok(());
                }
                if matches!(delta_type, "signature_delta" | "citations_delta") {
                    return Ok(());
                }
                let payload_field = match delta_type {
                    "text_delta" => "text",
                    "thinking_delta" => "thinking",
                    "input_json_delta" => "partial_json",
                    other => {
                        self.warn_unknown("stream delta", other);
                        return Ok(());
                    }
                };
                // A discarded known block still requires a well-formed known
                // delta. Only an explicitly ignored future block bypasses its
                // payload, above; budget truncation must not hide corruption.
                if !delta[payload_field].is_string() {
                    return Err(if delta_type == "input_json_delta" {
                        "Claude tool input delta has no string payload"
                    } else {
                        "Claude text delta has no string payload"
                    }
                    .into());
                }
                let Some(Block {
                    item,
                    input: streamed_input,
                }) = self
                    .blocks
                    .get_mut(&(ScopedId::new(Some(&parent), &stream.message), index))
                else {
                    if stream.truncated {
                        return Ok(());
                    }
                    return Err("Claude delta arrived without its content block".into());
                };
                if matches!(&item.content, ItemContent::Tool { .. })
                    && self
                        .tools
                        .get(&ScopedId::new(Some(&parent), &item.id))
                        .is_some_and(|old| {
                            old.content
                                .status()
                                .is_some_and(|status| status != ToolStatus::Running)
                        })
                {
                    return Ok(());
                }
                match (&mut item.content, delta_type) {
                    (ItemContent::Text { text, thinking, .. }, "text_delta" | "thinking_delta") => {
                        let value = delta[if *thinking { "thinking" } else { "text" }]
                            .as_str()
                            .ok_or("Claude text delta has no string payload")?;
                        if text.len().saturating_add(value.len()) > MAX_PAYLOAD {
                            return Err("Claude streamed text exceeds the 30 MiB item limit".into());
                        }
                        text.push_str(value);
                        step.events.push(Event::TextDelta {
                            id: item.id.clone(),
                            role: Role::Assistant,
                            thinking: *thinking,
                            delta: value.into(),
                        });
                    }
                    (ItemContent::Tool { input, .. }, "input_json_delta") => {
                        let value = string(delta, "partial_json");
                        if input.len().saturating_add(value.len()) > MAX_PAYLOAD {
                            return Err(
                                "Claude streamed tool input exceeds the 30 MiB item limit".into()
                            );
                        }
                        input.push_str(value);
                        streamed_input.push(value);
                        step.events
                            .push(Event::Item(streamed(item, streamed_input, &self.cwd)));
                    }
                    (_, other) => step.events.push(Event::Error {
                        message: format!("Unsupported Claude stream delta {other}"),
                        fatal: false,
                    }),
                }
            }
            "content_block_stop" => {
                let message = self
                    .streams
                    .get(&parent)
                    .ok_or("Claude block stop arrived before message_start")?
                    .message
                    .clone();
                let index = event["index"]
                    .as_u64()
                    .ok_or("Claude block stop has no index")?;
                if self
                    .ignored_blocks
                    .remove(&(ScopedId::new(Some(&parent), &message), index))
                {
                    return Ok(());
                }
                let key = (ScopedId::new(Some(&parent), &message), index);
                if let Some(mut item) = self.blocks.get(&key).map(|block| block.item.clone()) {
                    if self.completed_tool(Some(&parent), &item.id) {
                        return Ok(());
                    }
                    if let ItemContent::Tool { name, input, .. } = &item.content {
                        let parsed = if input.is_empty() {
                            Value::Null
                        } else if let Ok(parsed) = serde_json::from_str::<Value>(input) {
                            parsed
                        } else {
                            // Claude Code closes a block cut off by a retried
                            // request with a synthetic stop. Its call was
                            // discarded, never run, so it leaves the transcript.
                            let key = ScopedId::new(Some(&parent), &item.id);
                            self.forget_tool(&key);
                            self.tool_order.retain(|known| known != &key);
                            step.events.push(Event::ItemsRemoved {
                                ids: vec![item.id.clone()],
                            });
                            return Ok(());
                        };
                        // Classifying a tool needs its whole input, which only
                        // exists once the block has stopped, so the fragments
                        // accumulate as a plain tool call and the richer
                        // mapping happens here rather than against a prefix.
                        let content = tool_content(name, &parsed, &self.cwd);
                        self.remember_tool_input(Some(&parent), &item.id, &parsed)?;
                        if matches!(content, ItemContent::Changes { .. }) {
                            self.edit_tools
                                .insert(ScopedId::new(Some(&parent), &item.id), name.clone());
                        }
                        // The view last saw the call as it streamed, which a
                        // plain tool call already matches.
                        let shown = self
                            .blocks
                            .get(&key)
                            .map(|block| streamed(&block.item, &block.input, &self.cwd).content);
                        item.content = content;
                        if shown.as_ref() != Some(&item.content) {
                            step.events.push(Event::Item(item.clone()));
                        }
                        self.tool(Some(&parent), item);
                    }
                }
            }
            "message_delta" => {
                if parent.is_empty() {
                    self.update_usage(&event["usage"], step);
                }
            }
            "message_stop" => {
                if let Some(stream) = self.streams.remove(&parent) {
                    self.settle(Some(&parent), &stream.message);
                }
            }
            "ping" => {}
            "error" => {
                let message = content_text(&event["error"]);
                if !parent.is_empty() {
                    // Transient child detail is cleared by terminal task
                    // notifications. Keep explicit API failures in that child's
                    // transcript as well, with the same exact original cause.
                    step.events.push(Event::Item(child_failure_item(
                        frame,
                        "stream-error",
                        message.clone(),
                    )));
                }
                step.events.push(Event::Error {
                    message,
                    fatal: false,
                });
            }
            other => self.warn_unknown("stream event", other),
        }
        Ok(())
    }
    /// Forget a message that will stream no further blocks.
    fn settle(&mut self, parent: Option<&str>, message: &str) {
        let key = ScopedId::new(parent, message);
        self.blocks.retain(|(id, _), _| id != &key);
        self.ignored_blocks.retain(|(id, _)| id != &key);
    }

    fn permission_denied(
        &self,
        parent: Option<&str>,
        frame: &Value,
        step: &mut Step,
    ) -> Result<(), String> {
        let parent = parent.filter(|id| !id.is_empty());
        let id = required(frame, "tool_use_id")?;
        if self
            .dismissed_denials
            .iter()
            .any(|dismissed| dismissed == id)
        {
            return Ok(());
        }
        let name = required(frame, "tool_name")?;
        let input = if frame["tool_input"].is_object() {
            Some(frame["tool_input"].to_string())
        } else if parent.is_some() || string(frame, "agent_id").is_empty() {
            self.tool_inputs.get(&ScopedId::new(parent, id)).cloned()
        } else {
            // An agent UUID is not a parent tool-use ID. Without the latter,
            // no cached input can safely be attributed to this advisory.
            None
        }
        .filter(|input| input.len() <= chat::PermissionDenial::INPUT_LIMIT);
        let bounded = |value: &str| {
            let mut text = clean_permission_text(value);
            text.truncate(text.floor_char_boundary(4096));
            text
        };
        step.events
            .push(Event::PermissionDenied(chat::PermissionDenial {
                tool_use_id: bounded(id),
                tool_name: bounded(name),
                tool_input: input,
                reason: bounded(
                    frame["decision_reason"]
                        .as_str()
                        .unwrap_or(string(frame, "message")),
                ),
                reason_type: frame["decision_reason_type"].as_str().map(bounded),
                agent_id: frame["agent_id"].as_str().map(bounded),
            }));
        Ok(())
    }

    fn system(&mut self, frame: &Value, step: &mut Step) -> Result<(), String> {
        let subtype = required(frame, "subtype")?;
        // Both initial metadata and autonomous plan-mode transitions report
        // the effective permission mode, independent of our control replies.
        if let Some(mode) = frame["permissionMode"].as_str() {
            step.events.push(Event::PermissionMode(Some(
                if mode == "default" { "manual" } else { mode }.into(),
            )));
        }
        match subtype {
            "permission_denied" => {
                self.permission_denied(frame["parent_tool_use_id"].as_str(), frame, step)?
            }
            "init" | "commands_changed" => self.metadata(frame, step),
            "model_refusal_fallback" => {
                self.retract(&frame["retracted_message_uuids"], step)?;
                step.events.push(Event::Item(text_item(
                    required(frame, "uuid")?.into(),
                    Role::System,
                    frame.to_string(),
                    false,
                )));
            }
            "session_state_changed" => {
                let state = string(frame, "state");
                step.events.push(Event::Waiting(state == "requires_action"));
                match state {
                    "idle" => self.status(false, "Claude is idle", step),
                    "requires_action" => {
                        self.start_turn(step);
                        self.status(true, "Claude is waiting for your response", step)
                    }
                    state => {
                        self.start_turn(step);
                        self.status(true, format!("Claude: {state}"), step);
                    }
                }
            }
            "status" => step.events.push(Event::Status {
                busy: self.busy,
                detail: format!("Claude: {}", string(frame, "status")),
            }),
            "local_command_output" => step.events.push(Event::Item(text_item(
                required(frame, "uuid")?.into(),
                Role::System,
                content_text(&frame["content"]),
                false,
            ))),
            "background_tasks_changed" => self.background_level(frame, step),
            "task_started" | "task_progress" | "task_notification" | "task_updated" => {
                let task = required(frame, "task_id")?;
                // A worker's helper, or work a helper started: its own
                // transcript shows it.
                if self.nested_tasks.contains(task)
                    || frame["spawn_depth"].as_u64().is_some_and(|depth| depth > 1)
                    || frame["parent_task_id"]
                        .as_str()
                        .is_some_and(|parent| self.nested_tasks.contains(parent))
                {
                    self.nested_tasks.insert(task);
                    return Ok(());
                }
                let started = string(frame, "subtype") == "task_started";
                let Ok(known_tool) = self.task_agent(task, frame["tool_use_id"].as_str(), started)
                else {
                    // Contradictory advisory identities do not retarget an
                    // existing owner or make the conversation fail. A helper's
                    // later frames name only its task, so remember that too.
                    if frame["spawn_depth"].as_u64() != Some(1)
                        && frame["tool_use_id"]
                            .as_str()
                            .is_some_and(|tool| self.nested_call(tool))
                    {
                        self.nested_tasks.insert(task);
                    }
                    return Ok(());
                };
                let known_tool = known_tool.or_else(|| {
                    // Preserve unresolved local-agent lifecycle metadata and
                    // its fallback task row before the Agent tool arrives.
                    (started && string(frame, "task_type") == "local_agent")
                        .then(|| {
                            frame["tool_use_id"]
                                .as_str()
                                .filter(|tool| !tool.is_empty())
                                .map(str::to_owned)
                        })
                        .flatten()
                });
                if started {
                    let remember = |tasks: &mut VecDeque<String>, kept: bool| {
                        tasks.retain(|known| known != task);
                        if kept {
                            tasks.push_back(task.into());
                            if tasks.len() > MAX_TOOLS {
                                tasks.pop_front();
                            }
                        }
                    };
                    remember(
                        &mut self.command_tasks,
                        string(frame, "task_type") == "local_bash",
                    );
                    if string(frame, "task_type") == "local_bash"
                        && let Some(tool) = frame["tool_use_id"].as_str()
                    {
                        self.command_task_call(task, tool);
                    }
                    remember(
                        &mut self.subagent_tasks,
                        frame["owned_by_subagent"].as_bool() == Some(true),
                    );
                    remember(
                        &mut self.foreground_tasks,
                        string(frame, "task_type") == "local_bash"
                            && frame["is_backgrounded"].as_bool() == Some(false),
                    );
                }
                let command = self.command_tasks.iter().any(|known| known == task);
                // A subagent's task is work in that subagent's transcript,
                // which shows the call that started it; its notification is
                // delivered to that subagent rather than to this conversation.
                let subagent = self.subagent_tasks.iter().any(|known| known == task);
                // The call that started a task other than an agent, such as a
                // background command or a Monitor, is already its row.
                let call = known_tool
                    .is_none()
                    .then(|| frame["tool_use_id"].as_str())
                    .flatten()
                    .filter(|tool| self.task_call(tool).is_some());
                let id = format!("claude-task:{task}");
                let previous = self
                    .tools
                    .get(&ScopedId::new(None, &id))
                    .filter(|item| {
                        !started || matches!(item.content, ItemContent::Command { .. }) == command
                    })
                    .cloned();
                let mut item = previous.unwrap_or_else(|| Item {
                    id,
                    role: Role::Assistant,
                    // Shown only while nothing else stands for the task: a
                    // command is the command it ran, as far as its
                    // description tells, and anything else the task it is.
                    content: if command {
                        command_content(
                            String::new(),
                            String::new(),
                            self.cwd.to_string_lossy().into_owned(),
                            CommandAction::Run,
                        )
                    } else {
                        ItemContent::Tool {
                            name: "Subagent task".into(),
                            input: String::new(),
                            output: String::new(),
                            status: ToolStatus::Running,
                        }
                    },
                    started_at: None,
                    at: None,
                });
                let patch = if frame["patch"].is_object() {
                    &frame["patch"]
                } else {
                    frame
                };
                if patch["is_backgrounded"].as_bool() == Some(true) {
                    self.foreground_tasks.retain(|known| known != task);
                }
                let foreground = self.foreground_tasks.iter().any(|known| known == task);
                if string(frame, "subtype") == "task_started"
                    && matches!(string(frame, "task_type"), "local_agent" | "local_workflow")
                {
                    if string(frame, "task_type") == "local_agent"
                        && let Some(tool) = frame["tool_use_id"].as_str()
                    {
                        self.agent_task_calls.retain(|(known, _)| known != tool);
                        self.agent_task_calls.push_back((tool.into(), task.into()));
                        if self.agent_task_calls.len() > MAX_TOOLS {
                            self.agent_task_calls.pop_front();
                        }
                    }
                    if !self.tasks.iter().any(|running| running == task) {
                        self.tasks.push_back(task.into());
                    }
                    // A task whose completion never arrived must not end the
                    // conversation; the oldest is the likeliest to be one.
                    if self.tasks.len() > MAX_TOOLS {
                        self.tasks.pop_front();
                    }
                }
                let notification = string(frame, "subtype") == "task_notification";
                let waiting = known_tool
                    .as_deref()
                    .is_some_and(|tool| self.waiting_agents.contains(tool));
                let ended = task_status(string(patch, "status"), notification).map(|status| {
                    if notification && interim_notice(string(patch, "status"), patch) {
                        ToolStatus::Waiting
                    } else if !notification && waiting && status == ToolStatus::Succeeded {
                        // The order of these against the notice isn't known, so
                        // only a notice can say a waiting agent has finished
                        // its work. A stop or an interrupt still ends it, with
                        // a `killed` patch and no notice of its own.
                        ToolStatus::Waiting
                    } else {
                        status
                    }
                });
                if ended.is_some() {
                    self.tasks.retain(|running| running != task);
                    self.foreground_tasks.retain(|known| known != task);
                }
                let resumed = ended.is_none() && waiting;
                let fields = match &mut item.content {
                    ItemContent::Tool {
                        input,
                        output,
                        status,
                        ..
                    } => {
                        if let Some(text) = patch["description"].as_str() {
                            *input = text.into();
                        }
                        Some((output, status))
                    }
                    ItemContent::Command {
                        label,
                        command,
                        output,
                        status,
                        exit_code,
                        ..
                    } => {
                        if let Some(summary) = patch["summary"].as_str() {
                            *exit_code = summary_exit_code(summary).or(*exit_code);
                        }
                        // Claude Code names a command that has no description
                        // by the command itself. A description of several
                        // lines is that command, kept whole where the row's
                        // detail and copy show it rather than cut to the
                        // heading's one line.
                        if let Some(text) = patch["description"].as_str() {
                            if text.trim().contains('\n') {
                                label.clear();
                                *command = text.trim().into();
                            } else {
                                *label = text.into();
                            }
                        }
                        Some((output, status))
                    }
                    _ => None,
                };
                if let Some((output, status)) = fields {
                    // A foreground command's summary repeats the command.
                    if let Some(summary) = patch["summary"].as_str()
                        && !foreground
                    {
                        *output = summary.into();
                    }
                    // `SendMessage` restarts a finished agent under the same task.
                    *status =
                        ended.unwrap_or(if resumed || string(frame, "subtype") == "task_started" {
                            ToolStatus::Running
                        } else {
                            *status
                        });
                }
                // Lifecycle bookkeeping and notification summaries still need
                // the synthetic tool. Only its duplicate presentation is hidden.
                // A subagent's command is shown in its transcript when Claude
                // Code forwards the call; until then this row is all that
                // shows it ran, or how it failed.
                let represented = call.is_some()
                    || known_tool
                        .as_deref()
                        .or_else(|| frame["tool_use_id"].as_str())
                        .is_some_and(|tool| self.agent_tool(tool));
                // A restart under a call that isn't the Agent's own, or after it
                // ended, runs apart from any turn: no tool result will settle it.
                if started
                    && let Some(tool) = &known_tool
                    && (frame["tool_use_id"].as_str() != Some(tool.as_str())
                        || self.ended_agent(tool).is_some()
                        // Its foreground result has already been delivered.
                        || self
                            .tools
                            .get(&ScopedId::new(None, tool))
                            .and_then(|call| call.content.status())
                            .is_some_and(|status| status != ToolStatus::Running))
                {
                    self.background_agents.insert(tool.clone());
                }
                let failure_detail = (ended == Some(ToolStatus::Failed)).then(|| {
                    bounded_failure_detail([string(patch, "summary"), string(patch, "result")])
                });
                let status = self.update_task_agent(
                    task,
                    known_tool.as_deref(),
                    started,
                    ended,
                    item.content.status().unwrap_or(ToolStatus::Running),
                    failure_detail.as_deref().or_else(|| {
                        (ended == Some(ToolStatus::Waiting)).then_some(WAITING_ON_BACKGROUND)
                    }),
                    step,
                );
                if let ItemContent::Tool {
                    status: current, ..
                }
                | ItemContent::Command {
                    status: current, ..
                } = &mut item.content
                {
                    *current = status;
                }
                self.tool(None, item.clone());
                if let (Some(tool), Some(ended)) = (frame["tool_use_id"].as_str(), ended)
                    && known_tool.is_none()
                    && (notification || patch["status"].is_string())
                {
                    let outcome = TaskOutcome::new(ended, string(patch, "summary"));
                    if foreground {
                        // The call's own result settles it, unless that result
                        // says a timeout moved the command to the background
                        // after all: the acknowledgement then needs this.
                        self.remember_outcome(tool, outcome.recorded());
                    } else {
                        self.settle_task_call(tool, outcome, step);
                    }
                }
                if represented {
                    if self.visible_task_rows.remove(&item.id) {
                        step.events.push(Event::ItemsRemoved {
                            ids: vec![item.id.clone()],
                        });
                    }
                } else {
                    self.visible_task_rows.insert(item.id.clone());
                    step.events.push(Event::Item(item.clone()));
                }
                if string(frame, "subtype") == "task_started" {
                    // A new task identity binding retires any prior native
                    // origin, including reuse for a Bash or unknown task.
                    self.report_tasks.retain(|(known, ..)| known != task);
                    if matches!(string(frame, "task_type"), "local_agent" | "local_workflow") {
                        let kind = if string(frame, "task_type") == "local_workflow" {
                            ReportTask::Workflow
                        } else {
                            ReportTask::Agent
                        };
                        self.report_tasks.push_back((
                            task.into(),
                            kind,
                            string(frame, "description").trim().into(),
                        ));
                        if self.report_tasks.len() > MAX_TOOLS {
                            self.report_tasks.pop_front();
                        }
                    }
                }
                // Headless Claude Code tells the model a task finished in a turn
                // it never streams, then acts on it; without the notice the reply
                // would answer nothing on screen. The transcript records that turn,
                // and history shows it the same way. A notification the harness
                // keeps out of the transcript, or out of the model's way, is not
                // one the model acts on, and history never shows it.
                if notification && !subagent && !foreground {
                    let report = self
                        .report_tasks
                        .iter()
                        .find(|(known, ..)| known == task)
                        .map(|(_, kind, name)| (*kind, name.clone()));
                    let (kind, description) = match report {
                        Some((kind, name)) => (Some(kind), name),
                        None => (
                            None,
                            match &item.content {
                                ItemContent::Tool { input, .. } => input.clone(),
                                ItemContent::Command { label, .. } => label.clone(),
                                _ => String::new(),
                            },
                        ),
                    };
                    let recorded = ["skip_transcript", "ambient"]
                        .iter()
                        .all(|key| frame.get(key).and_then(Value::as_bool) != Some(true));
                    if recorded {
                        let mut content = live_notification(frame, &description, kind);
                        self.command_delivery(
                            &mut content,
                            string(frame, "tool_use_id"),
                            string(frame, "status"),
                            string(frame, "summary"),
                        );
                        let content = self.deduplicate(content);
                        step.events.push(Event::Item(Item {
                            id: format!("claude-notification:{task}:{}", string(frame, "uuid")),
                            role: Role::User,
                            content,
                            started_at: None,
                            at: None,
                        }));
                    }
                }
            }
            // Compaction is the one frame here a reader acts on, and its only
            // visible effect is the context figure. Nothing else on the wire
            // announces that the figure has moved, so the boundary asks for
            // the usage again exactly as a refresh does, rather than
            // narrating itself into the transcript. The summary that follows
            // marks the boundary there, as the reopened transcript does.
            "compact_boundary" => {
                // A subagent compacts its own context, not the conversation.
                self.compaction_summary_due |= frame["parent_tool_use_id"]
                    .as_str()
                    .is_none_or(str::is_empty);
                let usage = self.control(json!({"subtype":"get_context_usage"}))?;
                step.writes.extend(usage.writes);
            }
            // Auxiliary session frames are not transcript data. Each one used
            // to reach the transcript as a system item holding the raw JSON of
            // the frame, so a session with hooks, a retry or a background task
            // narrated its own plumbing at the reader - and the surface pinned
            // the newest of them above the conversation as though it were the
            // chat's subtitle. A failure still reaches the reader through the
            // error surface, and what the session is doing is the status
            // line's job.
            "informational"
            | "notification"
            | "hook_started"
            | "hook_progress"
            | "hook_response"
            | "api_retry"
            | "thinking_tokens"
            | "vcs_state_changed"
            | "elicitation_complete" => {}
            other => self.warn_unknown("system", other),
        }
        Ok(())
    }
}

/// A subagent's sidecar metadata, which is small and never a link.
fn sidecar_meta(path: &Path) -> Option<Value> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file
        .metadata()
        .is_ok_and(|meta| meta.is_file() && meta.len() <= 16 * 1024)
    {
        return None;
    }
    serde_json::from_reader(file).ok()
}

// Port of the official SDK's _build_conversation_chain, with a bounded offset
// index rather than retaining the full transcript. Never scan other sessions.
struct History {
    file: File,
    chain: Vec<Link>,
    offset: usize,
    session_id: String,
    subagent_dir: Option<PathBuf>,
    /// Each Agent tool call, with the order it was made in.
    agent_tools: HashMap<String, usize>,
    /// Provider IDs for historical call occurrences that needed a scope.
    agent_raw_tools: HashMap<String, String>,
    /// The agent ID Claude Code gave each background agent, which names its
    /// sidecar files.
    agent_ids: HashMap<String, String>,
    /// The agent ID Claude Code gave each agent, in the foreground or the
    /// background, which a later `SendMessage` or `TaskStop` addresses it by.
    agent_addresses: HashMap<String, String>,
    /// The name each Agent call asked its agent to go by. A sidecar records
    /// only the agent's type, which must not rename it on resume.
    agent_names: HashMap<String, String>,
    agent_status: HashMap<String, ToolStatus>,
    /// Latest actual user prompt on the displayed chain; provider notices and
    /// tool-result carriers never advance the current failure scope.
    latest_prompt: Option<u64>,
    background_agent_tools: HashSet<String>,
    /// How the background work other calls started ended, by call, as the
    /// transcript's notifications recorded it. One entry per notified call,
    /// each with a bounded summary, so that any page replayed later can tell
    /// a launch that was settled from one that never was.
    call_outcomes: HashMap<String, TaskOutcome>,
    /// Result carriers' ancestor calls across the visible branches. Offset
    /// metadata only; no input or patch bodies are retained.
    result_calls: history_branches::ResultCalls,
    tool_identities: history_branches::ToolIdentities,
    /// Messages in this conversation that were skipped because they were
    /// damaged, such as cut short by a crash.
    damaged: usize,
    /// Messages in this conversation too large to show, such as a huge tool
    /// output.
    oversized: usize,
    /// Whether each skipped message is marked where it was. One whose
    /// parent could not be read has no known place.
    marked: bool,
}
/// One step of the conversation, oldest first once indexed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    /// A record's offset, length and first content block.
    Record(u64, u64, usize),
    /// Where messages that could not be shown were.
    Gap(Gap),
}
/// Messages skipped at one place in a conversation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Gap {
    /// Where the first of them was written, which names the gap.
    at: u64,
    damaged: usize,
    oversized: usize,
}
impl Link {
    fn offset(&self) -> Option<u64> {
        match self {
            Self::Record(offset, ..) => Some(*offset),
            Self::Gap(_) => None,
        }
    }
}
impl Gap {
    fn add(&mut self, skipped: &Skipped) {
        if skipped.oversized {
            self.oversized += 1;
        } else {
            self.damaged += 1;
        }
    }
    fn is_empty(&self) -> bool {
        self.damaged + self.oversized == 0
    }
}
/// A record that could not be read, with what its start still says. Claude
/// Code writes a record's parent, branch and kind before its content, so a
/// record cut short, or one too large to read, usually still names them.
#[derive(Debug, Default)]
struct Skipped {
    /// Where the record started.
    at: usize,
    uuid: Option<String>,
    /// The record's parent, `Some(None)` when it recorded none.
    parent: Option<Option<String>>,
    logical_parent: Option<String>,
    /// The record's `type`, or its message's role when its type, written
    /// after the message, was lost.
    kind: Option<String>,
    subtype: Option<String>,
    sidechain: bool,
    team: bool,
    oversized: bool,
}
impl Skipped {
    /// Whether the record could have been a message shown in this
    /// conversation: a cut progress or system record, or a subagent's or
    /// teammate's message, loses nothing the reader would have seen.
    fn shown(&self) -> bool {
        !self.sidechain
            && !self.team
            && self
                .kind
                .as_deref()
                .is_none_or(|kind| matches!(kind, "user" | "assistant"))
    }
    /// Whether the record began a compacted conversation, which leads back
    /// to the turns it summarised rather than to a parent.
    fn compaction(&self) -> bool {
        self.parent == Some(None)
            && (self.subtype.as_deref() == Some("compact_boundary")
                || self.logical_parent.is_some())
    }
}
/// Reads what the start of a record that could not be read still says.
fn skipped_record(bytes: &[u8], at: usize, oversized: bool) -> Skipped {
    struct Record<'a>(&'a mut Skipped);
    impl<'de> serde::de::DeserializeSeed<'de> for Record<'_> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
            deserializer.deserialize_map(self)
        }
    }
    impl<'de> serde::de::Visitor<'de> for Record<'_> {
        type Value = ();
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a transcript record")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            use serde::de::IgnoredAny;
            let record = self.0;
            // Each field is kept as soon as it is read, so whatever came
            // before the point the record was cut at survives the error.
            while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                match &*key {
                    "parentUuid" => record.parent = Some(map.next_value()?),
                    "logicalParentUuid" => record.logical_parent = map.next_value()?,
                    "uuid" => record.uuid = map.next_value()?,
                    "type" => record.kind = map.next_value()?,
                    "subtype" => record.subtype = map.next_value()?,
                    "isSidechain" => {
                        record.sidechain = map.next_value::<Option<bool>>()? == Some(true);
                    }
                    "teamName" => {
                        record.team = map
                            .next_value::<Option<String>>()?
                            .is_some_and(|team| !team.is_empty());
                    }
                    "message" => map.next_value_seed(Message(&mut record.kind))?,
                    // An attachment's or progress record's body comes before
                    // its type, and only those records have one, so a record
                    // cut inside it is known not to be a message.
                    "attachment" => {
                        record.kind.get_or_insert_with(|| "attachment".into());
                        map.next_value::<IgnoredAny>()?;
                    }
                    "data" => {
                        record.kind.get_or_insert_with(|| "progress".into());
                        map.next_value::<IgnoredAny>()?;
                    }
                    _ => {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
            }
            Ok(())
        }
    }
    struct Message<'a>(&'a mut Option<String>);
    impl<'de> serde::de::DeserializeSeed<'de> for Message<'_> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
            deserializer.deserialize_any(self)
        }
    }
    impl<'de> serde::de::Visitor<'de> for Message<'_> {
        type Value = ();
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a message")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                if key == "role" {
                    let role = map.next_value::<Option<String>>()?;
                    // The record's own type, when it came first, decides.
                    if self.0.is_none() {
                        *self.0 = role;
                    }
                } else {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
            Ok(())
        }
        fn visit_unit<E>(self) -> Result<(), E> {
            Ok(())
        }
        fn visit_str<E>(self, _: &str) -> Result<(), E> {
            Ok(())
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(())
        }
    }
    let mut skipped = Skipped {
        at,
        oversized,
        ..Skipped::default()
    };
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let _ = serde::de::DeserializeSeed::deserialize(Record(&mut skipped), &mut deserializer);
    skipped
}
/// The skipped record that was a message's parent: the one with the parent's
/// UUID, or else, as Claude Code writes a record just after its parent, the
/// latest one written before the message, unless a main conversation message
/// came between them. A subagent's or teammate's record is never a main
/// conversation message's parent.
fn skipped_parent(
    skipped: &[Skipped],
    crossed: &[bool],
    written: &[(usize, &str)],
    parent: &str,
    before: usize,
) -> Option<usize> {
    let open = |index: &usize| {
        let record = &skipped[*index];
        !crossed[*index] && !record.sidechain && !record.team
    };
    if let Some(index) = (0..skipped.len())
        .filter(open)
        .find(|index| skipped[*index].uuid.as_deref() == Some(parent))
    {
        return Some(index);
    }
    let message = written[..written.partition_point(|(order, _)| *order < before)]
        .last()
        .map(|(order, _)| *order);
    (0..skipped.len())
        .rev()
        .filter(open)
        .find(|index| skipped[*index].at < before && skipped[*index].uuid.is_none())
        .filter(|index| message.is_none_or(|message| message < skipped[*index].at))
}
/// Why a conversation's saved history cannot be shown.
#[derive(Debug)]
enum HistoryUnavailable {
    /// There is no transcript, so Claude Code has nothing to resume either.
    Missing(super::SetupError),
    /// The transcript is there, but cagents cannot read it, for example
    /// because it is too large to index. Claude Code may still resume from
    /// it.
    Unreadable(super::SetupError),
}
type HistoryCalls = HashMap<usize, std::sync::Arc<Value>>;
#[derive(Debug)]
struct HistoryRecord {
    frame: Value,
    tool_calls: HistoryCalls,
    /// Messages that could not be shown at this point, in place of a record.
    gap: Option<Gap>,
}
impl std::ops::Deref for HistoryRecord {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.frame
    }
}
struct HistoryNode {
    parent: Option<String>,
    /// Whether the record belongs to the main conversation rather than to a
    /// subagent or teammate.
    main: bool,
    /// The parent this record was first written with, when a rewrite moved
    /// it. On resume Claude Code rewrites the messages a compaction kept
    /// with the summary as their parent; only this still leads to the turns
    /// before them.
    // Outer None means no parent rewrite; Some(None) preserves a root.
    first_parent: Option<Option<String>>,
    /// Where this record was first written; a rewrite moves `order`.
    first_order: usize,
    /// Where a compaction boundary leads back to. Compaction starts a new
    /// parent chain, so only this leads to the turns it summarised.
    compaction: Option<Compaction>,
    /// The record's part in a manual compaction, when it has one.
    compact_record: Option<CompactRecord>,
    visible: bool,
    message: bool,
    range: (u64, u64, usize),
    order: usize,
    tool_refs: Vec<(String, usize, bool)>,
    agents: Vec<history_branches::AgentCall>,
    result: history_branches::ResultDetails,
}
impl HistoryNode {
    fn original_parent(&self) -> Option<&str> {
        self.first_parent
            .as_ref()
            .unwrap_or(&self.parent)
            .as_deref()
    }
}
/// Claude Code records a manual compaction's summary before the `/compact`
/// the user typed; the live stream shows the command first.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CompactRecord {
    Summary,
    Command,
}
struct Compaction {
    /// The record Claude Code names as the boundary's predecessor.
    logical_parent: Option<String>,
    /// The main conversation's newest message before the boundary. Claude
    /// Code sometimes names a logical parent recorded after its boundary,
    /// within the new chain, or one it never kept; the summarised turns
    /// continue from here then.
    preceding: Option<String>,
}
impl History {
    /// The account configuration and resolved directory whose transcript a
    /// launch resumes. Only a launch that names neither exactly fails here.
    fn locate(launch: &Launch, id: &str) -> Result<(PathBuf, PathBuf), super::SetupError> {
        validate_id(id).map_err(super::SetupError::Invalid)?;
        let saved_cwd = &launch.recipe.working_directory;
        let cwd = match saved_cwd.canonicalize() {
            Ok(cwd) => cwd,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && saved_cwd.is_absolute()
                    && saved_cwd.components().all(|part| {
                        matches!(
                            part,
                            std::path::Component::RootDir | std::path::Component::Normal(_)
                        )
                    }) =>
            {
                saved_cwd.clone()
            }
            Err(error) => {
                return Err(super::SetupError::Unavailable(format!(
                    "Cannot resolve Claude session directory: {error}"
                )));
            }
        };
        let config = launch
            .recipe
            .environment
            .get("CLAUDE_CONFIG_DIR")
            .cloned()
            .filter(|s| !s.is_empty());
        let config = match config {
            Some(config) => {
                let path = PathBuf::from(config);
                if path.is_absolute() {
                    path
                } else {
                    cwd.join(path)
                }
            }
            None => {
                let home = launch
                    .recipe
                    .environment
                    .get("HOME")
                    .cloned()
                    .ok_or("Cannot locate Claude config without HOME")?;
                PathBuf::from(home).join(".claude")
            }
        };
        Ok((config, cwd))
    }

    fn open(config: &Path, cwd: &Path, id: &str) -> Result<Self, HistoryUnavailable> {
        let path = crate::agent::claude_transcript_for_start(config, cwd, id).map_err(|error| {
            let error = match error {
                crate::agent::ClaudeTranscriptError::Invalid(message) => {
                    super::SetupError::Invalid(message)
                }
                crate::agent::ClaudeTranscriptError::Unavailable(message) => {
                    super::SetupError::Unavailable(message)
                }
            };
            if crate::agent::claude_transcript_exists(config, cwd, id) {
                HistoryUnavailable::Unreadable(error)
            } else {
                HistoryUnavailable::Missing(error)
            }
        })?;
        let mut options = std::fs::OpenOptions::new();
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options.open(&path).map_err(|error| {
            let failure = super::SetupError::Unavailable(format!(
                "Cannot open exact Claude transcript: {error}"
            ));
            if error.kind() == std::io::ErrorKind::NotFound {
                HistoryUnavailable::Missing(failure)
            } else {
                HistoryUnavailable::Unreadable(failure)
            }
        })?;
        let mut history = Self::index_for_start(file, id, 32 * 1024 * 1024, 200_000)
            .map_err(HistoryUnavailable::Unreadable)?;
        history.subagent_dir = Some(path.with_extension("").join("subagents"));
        Ok(history)
    }
    #[cfg(test)]
    fn index(file: File, id: &str) -> Result<Self, String> {
        Self::index_with_tool_budget(file, id, 32 * 1024 * 1024, 200_000)
    }
    #[cfg(test)]
    fn index_with_tool_budget(
        file: File,
        id: &str,
        tool_bytes_limit: usize,
        tool_count_limit: usize,
    ) -> Result<Self, String> {
        Self::index_for_start(file, id, tool_bytes_limit, tool_count_limit)
            .map_err(|error| error.to_string())
    }
    fn index_for_start(
        mut file: File,
        id: &str,
        tool_bytes_limit: usize,
        tool_count_limit: usize,
    ) -> Result<Self, super::SetupError> {
        use std::io::BufRead as _;
        let metadata = file
            .metadata()
            .map_err(|e| super::SetupError::Unavailable(e.to_string()))?;
        if !metadata.is_file() || metadata.len() > 256 * 1024 * 1024 {
            return Err("Claude transcript must be a regular file smaller than 256 MiB".into());
        }
        let mut reader = std::io::BufReader::new(&mut file);
        let mut nodes = HashMap::<String, HistoryNode>::new();
        let mut parents = std::collections::HashSet::new();
        let mut message_offsets = HashMap::<String, usize>::new();
        let mut retracted = std::collections::HashSet::<String>::new();
        let mut task_notices = Vec::new();
        let mut user_prompts = HashSet::new();
        let mut main_message: Option<String> = None;
        let mut position = 0;
        let mut line = Vec::new();
        let mut tool_ref_bytes = 0_usize;
        let mut tool_ref_count = 0_usize;
        let mut skipped = Vec::<Skipped>::new();
        while position < metadata.len() {
            line.clear();
            let count = std::io::Read::by_ref(&mut reader)
                .take(MAX_PAYLOAD as u64 + 1)
                .read_until(b'\n', &mut line)
                .map_err(|e| super::SetupError::Unavailable(e.to_string()))?;
            if count == 0 {
                break;
            }
            if count > MAX_PAYLOAD {
                // An oversized record is a tool's output or an attachment;
                // skip the rest of it and keep the conversation readable.
                skipped.push(skipped_record(&line, position as usize, true));
                let mut length = count as u64;
                while line.last() != Some(&b'\n') {
                    line.clear();
                    let more = std::io::Read::by_ref(&mut reader)
                        .take(MAX_PAYLOAD as u64)
                        .read_until(b'\n', &mut line)
                        .map_err(|e| super::SetupError::Unavailable(e.to_string()))?;
                    if more == 0 {
                        break;
                    }
                    length += more as u64;
                }
                position += length;
                continue;
            }
            let mut record_offset = position;
            position += count as u64;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let (value, count) = match serde_json::from_slice::<Value>(&line) {
                Ok(value) => (value, count),
                // The last record, still being written by a conversation open
                // elsewhere, is not damage: it is read whole next time.
                Err(_) if position >= metadata.len() && line.last() != Some(&b'\n') => continue,
                Err(_) => {
                    // A record a crash cut short holds nothing that can be
                    // shown. Claude Code appends its next record to the same
                    // line, and that one is whole.
                    let trailing = trailing_record(&line);
                    let end = trailing.as_ref().map_or(line.len(), |(start, _)| *start);
                    skipped.push(skipped_record(&line[..end], record_offset as usize, false));
                    let Some((start, value)) = trailing else {
                        continue;
                    };
                    record_offset += start as u64;
                    (value, count - start)
                }
            };
            if value["sessionId"].as_str().is_some_and(|value| value != id) {
                return Err("Claude transcript contains another session identity".into());
            }
            let main = value["isSidechain"].as_bool() != Some(true)
                && value["teamName"].as_str().is_none_or(str::is_empty);
            let task_notice = recorded_notification(&value).is_some()
                || (string(&value, "type") == "system"
                    && matches!(
                        string(&value, "subtype"),
                        "task_started" | "task_progress" | "task_notification" | "task_updated"
                    ));
            if main && !task_notice && actual_user_prompt(&value) {
                user_prompts.insert(record_offset);
            }
            if main && task_notice {
                if task_notices.len() >= 200_000 {
                    return Err(
                        "Claude transcript exceeds the task notification index limit".into(),
                    );
                }
                task_notices.push((record_offset, count as u64, 0));
            }
            for field in ["supersedes", "retracted_message_uuids"] {
                if let Some(ids) = value[field].as_array() {
                    retracted.extend(ids.iter().filter_map(Value::as_str).map(str::to_owned));
                }
            }
            if !matches!(
                string(&value, "type"),
                "user" | "assistant" | "progress" | "system" | "attachment"
            ) {
                continue;
            }
            let Some(uuid) = value["uuid"].as_str() else {
                continue;
            };
            if nodes.len() >= 200_000 {
                return Err("Claude transcript exceeds the history index limit".into());
            }
            let message = matches!(string(&value, "type"), "user" | "assistant");
            // Legacy messages omitted parentUuid and followed file order.
            // An explicit null is a branch root, never an omitted link. A
            // rewritten legacy carrier keeps its already established parent.
            let parent = match value.get("parentUuid") {
                Some(parent) => parent.as_str().map(str::to_owned),
                None if main && message => nodes.get(uuid).map_or_else(
                    || main_message.clone().filter(|parent| parent != uuid),
                    |node| node.parent.clone(),
                ),
                None => None,
            };
            if let Some(parent) = &parent {
                parents.insert(parent.clone());
            }
            // A manual compaction's output repeats the summary shown above it.
            let visible = message
                && !model_only(&value)
                && main
                && value["repeatsCompactSummary"].as_bool() != Some(true);
            // A rewritten record keeps what it first said: where it was, the
            // parent it had, and the boundary it was.
            let previous = nodes.get_mut(uuid);
            let first_order = previous
                .as_ref()
                .map_or(record_offset as usize, |node| node.first_order);
            let first_parent = previous.as_ref().and_then(|node| {
                node.first_parent
                    .clone()
                    .or_else(|| (node.parent != parent).then(|| node.parent.clone()))
            });
            let compaction = previous
                .and_then(|node| node.compaction.take())
                .or_else(|| {
                    (parent.is_none()
                        && string(&value, "type") == "system"
                        && string(&value, "subtype") == "compact_boundary")
                        .then(|| Compaction {
                            logical_parent: value["logicalParentUuid"].as_str().map(str::to_owned),
                            preceding: main_message.clone(),
                        })
                });
            if message && main {
                main_message = Some(uuid.to_owned());
            }
            let compact_record = if !visible {
                None
            } else if value["isCompactSummary"].as_bool() == Some(true) {
                Some(CompactRecord::Summary)
            } else {
                value["message"]["content"]
                    .as_str()
                    .and_then(slash_command_prompt)
                    .is_some_and(|command| {
                        command
                            .split_once(' ')
                            .map_or(command.as_str(), |(name, _)| name)
                            == "/compact"
                    })
                    .then_some(CompactRecord::Command)
            };
            let message_id = value["message"]["id"].as_str().unwrap_or(uuid);
            let base = nodes
                .get(uuid)
                .map(|node| node.range.2)
                .unwrap_or_else(|| *message_offsets.get(message_id).unwrap_or(&0));
            message_offsets.insert(
                message_id.into(),
                base + value["message"]["content"].as_array().map_or(1, Vec::len),
            );
            let range = (record_offset, count as u64, base);
            let order = record_offset as usize;
            // Rewritten UUID carriers replace their index entry. Charge the
            // references actually retained, not all prior serialized carriers.
            if let Some(previous) = nodes.get(uuid) {
                tool_ref_count -= previous.tool_refs.len();
                tool_ref_bytes -=
                    history_branches::details_bytes(&previous.agents, &previous.result);
                tool_ref_bytes -= previous
                    .tool_refs
                    .iter()
                    .map(|(id, _, _)| id.len() + 64)
                    .sum::<usize>();
            }
            let mut tool_refs = Vec::new();
            if message
                && main
                && let Some(blocks) = value["message"]["content"].as_array()
            {
                for (index, block) in blocks.iter().enumerate() {
                    let result = string(block, "type") == "tool_result";
                    let tool = if result {
                        block["tool_use_id"].as_str()
                    } else if visible
                        && matches!(string(block, "type"), "tool_use" | "server_tool_use")
                    {
                        block["id"].as_str()
                    } else {
                        None
                    };
                    if let Some(tool) = tool {
                        tool_ref_bytes = tool_ref_bytes.saturating_add(tool.len() + 64);
                        tool_ref_count += 1;
                        if tool_ref_bytes > tool_bytes_limit || tool_ref_count > tool_count_limit {
                            return Err(
                                "Claude transcript exceeds the tool correlation index limit".into(),
                            );
                        }
                        tool_refs.push((tool.to_owned(), index, result));
                    }
                }
            }

            let (mut agents, result) = if message && main {
                history_branches::details(&value)
            } else {
                Default::default()
            };
            if !visible {
                agents.clear();
            }
            tool_ref_bytes =
                tool_ref_bytes.saturating_add(history_branches::details_bytes(&agents, &result));
            if tool_ref_bytes > tool_bytes_limit {
                return Err("Claude transcript exceeds the tool correlation index limit".into());
            }
            nodes.insert(
                uuid.into(),
                HistoryNode {
                    parent,
                    main,
                    first_parent,
                    first_order,
                    compaction,
                    compact_record,
                    visible,
                    message,
                    range,
                    order,
                    tool_refs,
                    agents,
                    result,
                },
            );
        }
        for id in retracted {
            if let Some(node) = nodes.get_mut(&id) {
                node.visible = false;
                node.tool_refs.clear();
                node.agents.clear();
                node.result = Default::default();
            }
        }
        let mut leaf: Option<&str> = None;
        let mut fallback: Option<&str> = None;
        for terminal in nodes.keys().filter(|key| !parents.contains(*key)) {
            let mut current = Some(terminal.as_str());
            let mut seen = std::collections::HashSet::new();
            while let Some(id) = current {
                if !seen.insert(id) {
                    break;
                }
                let Some(node) = nodes.get(id) else {
                    break;
                };
                if node.message {
                    if fallback.is_none_or(|leaf| nodes[leaf].order < node.order) {
                        fallback = Some(id);
                    }
                    if node.visible && leaf.is_none_or(|leaf| nodes[leaf].order < node.order) {
                        leaf = Some(id);
                    }
                    break;
                }
                current = node.parent.as_deref();
            }
        }
        leaf = leaf.or(fallback);
        // A skipped record whose later copy was read lost nothing.
        skipped.retain(|record| {
            record
                .uuid
                .as_ref()
                .is_none_or(|uuid| !nodes.contains_key(uuid))
        });
        // The main conversation's messages in file order, to find the one a
        // skipped record followed when its parent could not be read.
        let written = if skipped.is_empty() {
            Vec::new()
        } else {
            let mut written = nodes
                .iter()
                .filter(|(_, node)| node.message && node.main)
                .map(|(id, node)| (node.first_order, id.as_str()))
                .collect::<Vec<_>>();
            written.sort_unstable();
            written
        };
        let preceding = |at: usize, seen: &HashSet<&str>| {
            written[..written.partition_point(|(order, _)| *order < at)]
                .iter()
                .rev()
                .map(|(_, id)| *id)
                .find(|id| !seen.contains(id))
        };
        let mut crossed = vec![false; skipped.len()];
        let mut chain = Vec::new();

        let mut seen = std::collections::HashSet::new();
        let mut compacted = false;
        let mut newer_record = None;
        while let Some(id) = leaf {
            if !seen.insert(id) {
                // Turns already found before a compaction boundary that led
                // back into them stay readable; only the current chain
                // looping on itself is malformed.
                if compacted {
                    break;
                }
                return Err("Claude transcript contains a cyclic parent chain".into());
            }
            let Some(node) = nodes.get(id) else {
                break;
            };
            if node.visible {
                // The chain is built newest first: put a summary after the
                // `/compact` recorded just after it, where the user saw it.
                let position = if node.compact_record == Some(CompactRecord::Summary)
                    && newer_record == Some(CompactRecord::Command)
                {
                    chain.len() - 1
                } else {
                    chain.len()
                };
                let (offset, len, base) = node.range;
                // Gaps hold no tools, so the tools list counts from its end.
                chain.insert(position, Link::Record(offset, len, base));
                newer_record = node.compact_record;
            }
            leaf = match node.parent.as_deref() {
                // The parent was skipped. The conversation goes on across it,
                // and any skipped records it followed, from where they did,
                // rather than ending there and hiding everything earlier.
                Some(parent) if !nodes.contains_key(parent) && !skipped.is_empty() => {
                    let mut gap = Gap::default();
                    let mut missing = parent;
                    let mut before = node.first_order;
                    let next = loop {
                        let Some(index) =
                            skipped_parent(&skipped, &crossed, &written, missing, before)
                        else {
                            break None;
                        };
                        crossed[index] = true;
                        let record = &skipped[index];
                        if record.shown() {
                            gap.add(record);
                            gap.at = record.at as u64;
                        }
                        if record.compaction() {
                            compacted = true;
                            break record
                                .logical_parent
                                .as_deref()
                                .filter(|previous| {
                                    nodes
                                        .get(*previous)
                                        .is_some_and(|previous| previous.first_order < record.at)
                                })
                                .or_else(|| preceding(record.at, &seen));
                        }
                        match &record.parent {
                            Some(Some(parent)) if nodes.contains_key(parent) => {
                                break Some(parent.as_str());
                            }
                            Some(Some(parent)) => {
                                missing = parent;
                                before = record.at;
                            }
                            Some(None) => break None,
                            // Cut before it named its parent: it followed the
                            // message written before it.
                            None => break preceding(record.at, &seen),
                        }
                    };
                    if !gap.is_empty() {
                        chain.push(Link::Gap(gap));
                        newer_record = None;
                    }
                    next
                }
                Some(parent) => Some(parent),
                // A compaction boundary has no parent: the turns it summarised
                // continue through its logical parent, when that was recorded
                // before it, and otherwise from the message that preceded it.
                // Messages the compaction kept may already have been read after
                // the summary; the summarised turns continue before them.
                None => node.compaction.as_ref().and_then(|compaction| {
                    compacted = true;
                    let mut previous = compaction
                        .logical_parent
                        .as_deref()
                        .filter(|previous| {
                            nodes
                                .get(*previous)
                                .is_some_and(|previous| previous.first_order < node.first_order)
                        })
                        .or(compaction.preceding.as_deref())?;
                    let mut kept = HashSet::new();
                    while seen.contains(previous) {
                        if !kept.insert(previous) {
                            return None;
                        }
                        let node = nodes.get(previous)?;
                        previous = node.original_parent()?;
                    }
                    Some(previous)
                }),
            };
        }
        chain.reverse();

        // Keep each admitted skipped message at its own file position. Branch
        // records may fall between skipped ancestors from the active chain.
        let mut marked = true;
        let mut unplaced = Gap::default();
        let mut gaps = Vec::new();
        let mut parents = history_branches::Parents::new(&nodes, &skipped, &written);
        for (record, crossed) in skipped
            .iter()
            .zip(&crossed)
            .filter(|(record, _)| record.shown())
        {
            if !crossed {
                match &record.parent {
                    Some(Some(parent)) if parents.resolve(parent, record.at).is_none() => continue,
                    Some(_) => {}
                    None => {
                        marked = false;
                        unplaced.add(record);
                        continue;
                    }
                }
            }
            let mut gap = Gap {
                at: record.at as u64,
                ..Gap::default()
            };
            gap.add(record);
            gaps.push(gap);
        }
        let history_branches::Index {
            chain: records,
            result_calls,
            tool_identities,
            ambiguous,
        } = history_branches::index(&nodes, &seen, &mut parents)
            .map_err(super::SetupError::Invalid)?;
        gaps.sort_unstable_by_key(|gap| gap.at);
        let mut gaps = gaps.into_iter().peekable();
        let mut chain = Vec::with_capacity(records.len() + gaps.len());
        let append_gap = |chain: &mut Vec<Link>, gap: Gap| {
            if let Some(Link::Gap(previous)) = chain.last_mut() {
                previous.damaged += gap.damaged;
                previous.oversized += gap.oversized;
            } else {
                chain.push(Link::Gap(gap));
            }
        };
        for (offset, len, base) in records {
            while gaps.peek().is_some_and(|gap| gap.at < offset) {
                append_gap(&mut chain, gaps.next().unwrap());
            }
            chain.push(Link::Record(offset, len, base));
        }
        for gap in gaps {
            append_gap(&mut chain, gap);
        }
        let (damaged, oversized) = chain
            .iter()
            .filter_map(|link| match link {
                Link::Gap(gap) => Some(gap),
                Link::Record(..) => None,
            })
            .chain([&unplaced])
            .fold((0, 0), |(damaged, oversized), gap| {
                (damaged + gap.damaged, oversized + gap.oversized)
            });
        let latest_prompt = chain
            .iter()
            .filter_map(Link::offset)
            .filter(|offset| user_prompts.contains(offset))
            .max();
        let offset = chain.len();
        let mut history = Self {
            file,
            chain,
            offset,
            session_id: id.into(),
            subagent_dir: None,
            agent_tools: HashMap::new(),
            agent_raw_tools: HashMap::new(),
            agent_ids: HashMap::new(),
            agent_addresses: HashMap::new(),
            agent_names: HashMap::new(),
            agent_status: HashMap::new(),
            latest_prompt,
            background_agent_tools: HashSet::new(),
            call_outcomes: HashMap::new(),
            result_calls,
            tool_identities,
            damaged,
            oversized,
            marked,
        };
        history_branches::index_activity(&mut history, &nodes, &ambiguous, &task_notices)?;
        Ok(history)
    }
    /// A transcript records what ran, not proof of a process still running.
    /// Keep confirmed terminal outcomes; unresolved runs have ended for the
    /// newly attached process until live frames say
    /// otherwise. Explicit cancellation remains distinct.
    fn settle_stopped(&mut self) {
        for tool in self.agent_tools.keys() {
            let status = self
                .agent_status
                .entry(tool.clone())
                .or_insert(ToolStatus::Ended);
            if !status.is_live() {
                continue;
            }
            let stopped = self
                .subagent_dir
                .as_ref()
                .and_then(|dir| {
                    self.agent_ids
                        .get(tool)
                        .filter(|agent| {
                            !agent.is_empty()
                                && agent.bytes().all(|byte| byte.is_ascii_alphanumeric())
                        })
                        .and_then(|agent| {
                            sidecar_meta(&dir.join(format!("agent-{agent}.meta.json")))
                        })
                })
                .is_some_and(|meta| meta["stoppedByUser"].as_bool() == Some(true));
            *status = if stopped {
                ToolStatus::Stopped
            } else {
                ToolStatus::Ended
            };
        }
    }
    /// Load only children linked by a tool-use ID in this exact parent session.
    /// A bounded tail keeps reopening a long-running session cheap.
    ///
    /// Each child also carries how the background tasks it started ended, by
    /// the call that launched them, from every notification its tail
    /// recorded, in any of the forms Claude Code records one.
    #[allow(clippy::type_complexity)]
    fn subagent_frames(
        &self,
    ) -> Result<
        Vec<(
            String,
            String,
            String,
            Vec<Value>,
            Vec<(String, TaskOutcome)>,
        )>,
        String,
    > {
        use std::io::BufRead as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let Some(dir) = &self.subagent_dir else {
            return Ok(Vec::new());
        };
        match std::fs::symlink_metadata(dir) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => return Ok(Vec::new()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("Cannot inspect Claude subagents: {error}")),
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(Vec::new());
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("agent-") && name.ends_with(".meta.json"))
            })
            .collect::<Vec<_>>();
        paths.sort_by_key(|path| {
            std::cmp::Reverse(
                std::fs::symlink_metadata(path)
                    .ok()
                    .and_then(|meta| meta.modified().ok()),
            )
        });
        let mut children = Vec::new();
        for meta_path in paths.into_iter().take(32) {
            let Some(stem) = meta_path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".meta.json"))
            else {
                continue;
            };
            let agent_id = stem.strip_prefix("agent-").unwrap_or("");
            if agent_id.is_empty() || !agent_id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
                continue;
            }
            let Some(meta) = sidecar_meta(&meta_path) else {
                continue;
            };
            let Some(raw_parent) = meta["toolUseId"].as_str() else {
                continue;
            };
            let parent = if !self.agent_raw_tools.contains_key(raw_parent) {
                self.agent_tools
                    .get_key_value(raw_parent)
                    .map(|(tool, _)| tool.as_str())
            } else {
                let mut scoped = self
                    .agent_ids
                    .iter()
                    .filter(|(tool, agent)| {
                        agent.as_str() == agent_id
                            && self
                                .agent_raw_tools
                                .get(*tool)
                                .map_or(tool.as_str(), String::as_str)
                                == raw_parent
                    })
                    .map(|(tool, _)| tool.as_str());
                let first = scoped.next();
                if scoped.next().is_none() { first } else { None }
            };
            let Some(parent) = parent else {
                continue;
            };
            let path = dir.join(format!("{stem}.jsonl"));
            let Ok(mut file) = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)
            else {
                continue;
            };
            let Ok(info) = file.metadata() else { continue };
            if !info.is_file() || info.len() > 256 * 1024 * 1024 {
                continue;
            }
            let offset = info.len().saturating_sub(256 * 1024);
            if file.seek(SeekFrom::Start(offset)).is_err() {
                continue;
            }
            let mut reader = std::io::BufReader::new(file);
            let mut line = Vec::new();
            if offset > 0 && reader.read_until(b'\n', &mut line).is_err() {
                continue;
            }
            let mut frames = VecDeque::new();
            let mut outcomes = Vec::new();
            let mut retained_bytes = 0;
            loop {
                line.clear();
                let Ok(count) = reader
                    .by_ref()
                    .take(MAX_PAYLOAD as u64 + 1)
                    .read_until(b'\n', &mut line)
                else {
                    break;
                };
                if count == 0 {
                    break;
                }
                if count > MAX_PAYLOAD {
                    break;
                }
                let large = count > 48 * 1024;
                if large
                    && !line
                        .windows(NOTIFICATION_OPEN.len())
                        .any(|window| window == NOTIFICATION_OPEN.as_bytes())
                {
                    continue;
                }
                let Ok(frame) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                if frame["sessionId"].as_str() != Some(&self.session_id)
                    || frame["agentId"].as_str().is_some_and(|id| id != agent_id)
                {
                    continue;
                }
                if let Some(content) = recorded_notification(&frame) {
                    outcomes.extend(
                        notified_calls(&content)
                            .into_iter()
                            .map(|(tool, outcome)| (tool, outcome.recorded())),
                    );
                }
                if large
                    || frame["agentId"].as_str() != Some(agent_id)
                    || frame["isSidechain"].as_bool() != Some(true)
                    || !matches!(string(&frame, "type"), "user" | "assistant")
                {
                    continue;
                }
                retained_bytes += count;
                frames.push_back((frame, count));
                while frames.len() > 64 || retained_bytes > 48 * 1024 {
                    if let Some((_, bytes)) = frames.pop_front() {
                        retained_bytes -= bytes;
                    }
                }
            }
            children.push((
                parent.to_owned(),
                agent_label(
                    self.agent_names.get(parent).map_or("", String::as_str),
                    string(&meta, "description"),
                    string(&meta, "agentType"),
                )
                .into(),
                string(&meta, "description").into(),
                frames.into_iter().map(|(frame, _)| frame).collect(),
                outcomes,
            ));
        }
        Ok(children)
    }
    fn read_record_for_start(
        &mut self,
        offset: u64,
        len: u64,
        base: usize,
    ) -> Result<Value, super::SetupError> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| super::SetupError::Unavailable(e.to_string()))?;
        let mut bytes = vec![0; len as usize];
        self.file
            .read_exact(&mut bytes)
            .map_err(|e| super::SetupError::Unavailable(e.to_string()))?;
        let mut frame: Value = serde_json::from_slice(&bytes).map_err(|e| {
            super::SetupError::Unavailable(format!("Claude history changed while reading: {e}"))
        })?;
        // The provider can rewrite this inode after indexing. Revalidate the
        // record being published, including off-page tool-call dependencies.
        // A mismatch here invalidates this read, not the immutable launch:
        // another attempt can reindex the repaired or newly settled file.
        if frame["sessionId"]
            .as_str()
            .is_some_and(|id| id != self.session_id)
        {
            return Err(super::SetupError::Unavailable(
                "Claude history changed session identity while reading".into(),
            ));
        }
        let object = frame.as_object_mut().ok_or_else(|| {
            super::SetupError::Unavailable("Claude history message is not an object".into())
        })?;
        object.insert("_cagents_block_base".into(), json!(base));
        object.remove("supersedes");
        Ok(frame)
    }
    fn page(&mut self) -> Result<Vec<HistoryRecord>, String> {
        self.page_for_start().map_err(|error| error.to_string())
    }
    fn page_for_start(&mut self) -> Result<Vec<HistoryRecord>, super::SetupError> {
        let mut start = self.offset;
        let mut total = 0;
        let mut charged = HashSet::new();
        // A marker for messages that could not be shown takes no space, so
        // the first record a page holds is counted, not its first link.
        let mut holds_record = false;
        while start > 0 && self.offset - start < 100 {
            let Link::Record(offset, len, _) = self.chain[start - 1] else {
                start -= 1;
                continue;
            };
            let mut records = HashMap::new();
            if !charged.contains(&offset) {
                records.insert(offset, len);
            }
            if let Some(calls) = self.result_calls.get(&offset) {
                for &(_, (call, len, _, _)) in calls {
                    if !charged.contains(&call) {
                        records.entry(call).or_insert(len);
                    }
                }
            }
            let bytes: u64 = records.values().sum();
            // A normal oversized call/result pair remains readable, but a
            // pathological multi-result group cannot hydrate the entire log.
            if bytes > (MAX_PAYLOAD as u64) * 2 {
                return Err(
                    "Claude history tool dependency group exceeds the bounded page limit".into(),
                );
            }
            // Original records and required call dependencies share the page
            // byte budget. One oversized record/dependency group remains
            // loadable under the existing individual MAX_PAYLOAD limit.
            if total + bytes > HISTORY_PAGE && holds_record {
                break;
            }
            total += bytes;
            charged.extend(records.into_keys());
            holds_record = true;
            start -= 1;
        }
        let mut result = Vec::new();
        let mut positions = HashMap::new();
        for index in start..self.offset {
            let record = match self.chain[index] {
                Link::Record(offset, len, base) => {
                    positions.insert(offset, result.len());
                    HistoryRecord {
                        frame: self.read_record_for_start(offset, len, base)?,
                        tool_calls: HashMap::new(),
                        gap: None,
                    }
                }
                Link::Gap(gap) => HistoryRecord {
                    frame: Value::Null,
                    tool_calls: HashMap::new(),
                    gap: Some(gap),
                },
            };
            result.push(record);
        }
        let mut dependencies = std::collections::BTreeMap::<
            u64,
            (u64, usize, std::collections::BTreeSet<usize>),
        >::new();
        for offset in self.chain[start..self.offset]
            .iter()
            .filter_map(Link::offset)
        {
            if let Some(calls) = self.result_calls.get(&offset) {
                for &(_, (call, len, base, index)) in calls {
                    dependencies
                        .entry(call)
                        .or_insert_with(|| (len, base, Default::default()))
                        .2
                        .insert(index);
                }
            }
        }
        let mut call_blocks = HashMap::new();
        for (offset, (len, base, indices)) in dependencies {
            let mut original = if positions.contains_key(&offset) {
                None
            } else {
                Some(self.read_record_for_start(offset, len, base)?)
            };
            for index in indices {
                let block = if let Some(position) = positions.get(&offset) {
                    result[*position].frame["message"]["content"]
                        .as_array()
                        .and_then(|blocks| blocks.get(index))
                        .cloned()
                        .ok_or_else(|| {
                            super::SetupError::Unavailable(
                                "Claude indexed tool block changed while reading".into(),
                            )
                        })?
                } else {
                    let blocks = original.as_mut().unwrap()["message"]["content"]
                        .as_array_mut()
                        .ok_or_else(|| {
                            super::SetupError::Unavailable(
                                "Claude indexed tool call changed while reading".into(),
                            )
                        })?;
                    std::mem::take(blocks.get_mut(index).ok_or_else(|| {
                        super::SetupError::Unavailable(
                            "Claude indexed tool block changed while reading".into(),
                        )
                    })?)
                };
                if !matches!(string(&block, "type"), "tool_use" | "server_tool_use") {
                    return Err(super::SetupError::Unavailable(
                        "Claude indexed tool call changed while reading".into(),
                    ));
                }
                required(&block, "name").map_err(super::SetupError::Unavailable)?;
                call_blocks.insert((offset, index), std::sync::Arc::new(block));
            }
        }
        let mut aliased_calls = HashMap::new();
        for offset in self.chain[start..self.offset]
            .iter()
            .filter_map(Link::offset)
        {
            if let Some(calls) = self.result_calls.get(&offset) {
                let record = &mut result[positions[&offset]];
                for &(result_index, (call, _, _, call_index)) in calls {
                    let block = record.frame["message"]["content"]
                        .as_array()
                        .and_then(|blocks| blocks.get(result_index))
                        .ok_or_else(|| {
                            super::SetupError::Unavailable(
                                "Claude indexed tool result changed while reading".into(),
                            )
                        })?;
                    let context = call_blocks[&(call, call_index)].clone();
                    if string(block, "type") != "tool_result"
                        || required(block, "tool_use_id").map_err(super::SetupError::Unavailable)?
                            != required(&context, "id").map_err(super::SetupError::Unavailable)?
                    {
                        return Err(super::SetupError::Unavailable(
                            "Claude indexed tool result identity changed while reading".into(),
                        ));
                    }
                    let context =
                        if let Some(identity) = self.tool_identities.get(&(call, call_index)) {
                            aliased_calls
                                .entry((call, call_index))
                                .or_insert_with(|| {
                                    let mut block = (*context).clone();
                                    block["id"] = json!(identity);
                                    std::sync::Arc::new(block)
                                })
                                .clone()
                        } else {
                            context
                        };
                    record.tool_calls.insert(result_index, context);
                }
            }
        }
        for (&offset, &position) in &positions {
            if let Some(blocks) = result[position].frame["message"]["content"].as_array_mut() {
                for (index, block) in blocks.iter_mut().enumerate() {
                    if let Some(identity) = self.tool_identities.get(&(offset, index)) {
                        let field = if string(block, "type") == "tool_result" {
                            "tool_use_id"
                        } else {
                            "id"
                        };
                        block[field] = json!(identity);
                    }
                }
            }
        }
        self.offset = start;
        Ok(result)
    }
}

impl Claude {
    fn elicitation(&mut self, raw: Value) -> Result<Step, String> {
        let id = required(&raw, "request_id")?.to_owned();
        let request = &raw["request"];
        let mut step = Step::default();
        let fields = match string(request, "mode") {
            "url" => {
                let url = required(request, "url")?;
                let parsed = url::Url::parse(url)
                    .map_err(|_| "Claude elicitation supplied an invalid URL")?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    return Err("Claude elicitation URL must use HTTP(S)".into());
                }
                step.events.push(Event::OpenUrl {
                    url: url.into(),
                    instructions: string(request, "message").into(),
                });
                Ok(vec![])
            }
            "form" | "" => elicitation_fields(&request["requested_schema"]),
            mode => Err(format!("Unsupported Claude elicitation mode {mode}")),
        };
        let (fields, actions) = match fields {
            Ok(fields) => (
                fields,
                vec![
                    choice("accept", "Submit"),
                    choice("decline", "Decline"),
                    choice("cancel", "Cancel"),
                ],
            ),
            Err(message) => {
                step.events.push(Event::Error {
                    message,
                    fatal: false,
                });
                (
                    vec![],
                    vec![choice("decline", "Decline"), choice("cancel", "Cancel")],
                )
            }
        };
        step.events.push(Event::Request(Request {
            kind: chat::RequestKind::Consent,
            id: id.clone(),
            title: format!("Claude MCP: {}", string(request, "mcp_server_name")),
            description: string(request, "message").into(),
            fields,
            actions,
        }));
        self.pending.insert(id, raw);
        Ok(step)
    }
    fn answer_elicitation(
        &mut self,
        id: String,
        action: String,
        answers: Vec<Answer>,
    ) -> Result<Step, String> {
        let request = &self
            .pending
            .get(&id)
            .ok_or("Claude elicitation is no longer pending")?["request"];
        let response = match action.as_str() {
            "decline" | "cancel" => json!({"action":action}),
            "accept" => {
                if string(request, "mode") == "url" {
                    if !answers.is_empty() {
                        return Err("Claude URL elicitation does not accept form fields".into());
                    }
                    json!({"action":"accept"})
                } else {
                    let schema = &request["requested_schema"];
                    let fields = elicitation_fields(schema)?;
                    let mut content = serde_json::Map::new();
                    for answer in &answers {
                        if !fields.iter().any(|field| field.id == answer.field)
                            || content.contains_key(&answer.field)
                        {
                            return Err(
                                "Claude elicitation answer has an unknown or duplicate field"
                                    .into(),
                            );
                        }
                        let field_schema = &schema["properties"][&answer.field];
                        let value = elicitation_value(field_schema, &answer.values)?;
                        content.insert(answer.field.clone(), value);
                    }
                    if let Some(required) = schema["required"].as_array() {
                        for key in required {
                            if !content.contains_key(
                                key.as_str().ok_or("Malformed Claude required field")?,
                            ) {
                                return Err(format!("Missing required Claude field {key}"));
                            }
                        }
                    }
                    json!({"action":"accept", "content":content})
                }
            }
            _ => return Err("Claude elicitation expects accept, decline, or cancel".into()),
        };
        if encoded_len(&response)? >= MAX_PAYLOAD {
            return Err("Claude elicitation response must be smaller than 30 MiB".into());
        }
        let mut step = Step {
            suppress_current_notification: false,
            notification_preview: None,
            writes: vec![
                json!({"type":"control_response", "response":{"subtype":"success", "request_id":id, "response":response}}),
            ],
            events: vec![],
        };
        self.retire(id, &mut step);
        Ok(step)
    }
}

fn elicitation_fields(schema: &Value) -> Result<Vec<Field>, String> {
    let object = schema
        .as_object()
        .ok_or("Claude elicitation schema is not an object")?;
    if string(schema, "type") != "object"
        || object.keys().any(|k| {
            !matches!(
                k.as_str(),
                "type"
                    | "properties"
                    | "required"
                    | "additionalProperties"
                    | "title"
                    | "description"
                    | "$schema"
            )
        })
    {
        return Err("Claude elicitation uses an unsupported object schema; decline or use a supported surface".into());
    }
    let properties = schema["properties"]
        .as_object()
        .ok_or("Claude elicitation has no properties")?;
    if properties.len() > 32 {
        return Err("Claude elicitation has too many fields".into());
    }
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .ok_or("Claude elicitation required must be an array")?;
        if required
            .iter()
            .any(|key| key.as_str().is_none_or(|key| !properties.contains_key(key)))
        {
            return Err("Claude elicitation names an unknown required field".into());
        }
    }
    let mut fields = Vec::new();
    for (id, property) in properties {
        let fields_schema = property
            .as_object()
            .ok_or("Claude elicitation property is not an object")?;
        if fields_schema.keys().any(|k| {
            !matches!(
                k.as_str(),
                "type" | "title" | "description" | "enum" | "enumNames" | "items" | "default"
            )
        }) {
            return Err(format!(
                "Claude elicitation field {id} uses unsupported validation constraints"
            ));
        }
        let kind = string(property, "type");
        if !matches!(kind, "string" | "boolean" | "number" | "integer" | "array") {
            return Err(format!(
                "Claude elicitation field {id} has unsupported type {kind}"
            ));
        }
        if kind == "array"
            && (string(&property["items"], "type") != "string"
                || property["items"].as_object().is_none_or(|items| {
                    items.keys().any(|k| !matches!(k.as_str(), "type" | "enum"))
                }))
        {
            return Err("Claude elicitation supports arrays of strings only".into());
        }
        let enum_schema = if kind == "array" {
            &property["items"]
        } else {
            property
        };
        if enum_schema
            .get("enum")
            .is_some_and(|value| !value.is_array())
        {
            return Err("Claude elicitation enum must be an array".into());
        }
        let choices = if let Some(values) = enum_schema["enum"].as_array() {
            values
                .iter()
                .map(|value| {
                    let text = value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string());
                    choice(&text, &text)
                })
                .collect()
        } else if kind == "boolean" {
            vec![choice("true", "Yes"), choice("false", "No")]
        } else {
            vec![]
        };
        fields.push(Field {
            id: id.clone(),
            label: property["title"].as_str().unwrap_or(id).into(),
            description: string(property, "description").into(),
            choices,
            multiple: kind == "array",
            secret: false,
            initial: property["default"].as_str().unwrap_or("").into(),
        });
    }
    Ok(fields)
}
fn elicitation_value(schema: &Value, values: &[String]) -> Result<Value, String> {
    let kind = string(schema, "type");
    if kind != "array" && values.len() != 1 {
        return Err("Claude elicitation expects one value for this field".into());
    }
    let value = match kind {
        "string" => json!(values[0]),
        "boolean" => match values[0].as_str() {
            "true" => json!(true),
            "false" => json!(false),
            _ => return Err("Claude elicitation expects true or false".into()),
        },
        "integer" => json!(
            values[0]
                .parse::<i64>()
                .map_err(|_| "Claude elicitation expects an integer")?
        ),
        "number" => {
            let number = values[0]
                .parse::<f64>()
                .map_err(|_| "Claude elicitation expects a number")?;
            if !number.is_finite() {
                return Err("Claude elicitation expects a finite number".into());
            }
            json!(number)
        }
        "array" => {
            let enum_values = schema["items"]["enum"].as_array();
            if values
                .iter()
                .any(|v| enum_values.is_some_and(|allowed| !allowed.contains(&json!(v))))
            {
                return Err("Claude elicitation array contains a value outside its enum".into());
            }
            json!(values)
        }
        _ => return Err("Unsupported Claude elicitation field type".into()),
    };
    if schema["enum"]
        .as_array()
        .is_some_and(|allowed| !allowed.contains(&value))
    {
        return Err("Claude elicitation answer is outside its enum".into());
    }
    Ok(value)
}

mod history_branches;
#[cfg(test)]
mod history_tests;
mod streamed_input;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod startup_history_retryability_tests {
    use super::*;
    use std::io::Write;

    const SESSION: &str = "00000000-0000-4000-8000-000000000001";

    fn indexed(path: &Path) -> History {
        History::index_for_start(File::open(path).unwrap(), SESSION, 4096, 16).unwrap()
    }

    #[test]
    fn indexed_transcript_mutation_and_repair_keep_retryable_startup_errors() {
        let fixture = tempfile::NamedTempFile::new().unwrap();
        let original = format!(
            "{}\n",
            json!({
                "type":"user", "uuid":"prompt", "parentUuid":null, "sessionId":SESSION,
                "message":{"role":"user","content":"kept prompt"}
            })
        )
        .into_bytes();
        let mut malformed = original.clone();
        malformed[0] = b'[';
        let replaced = String::from_utf8(original.clone())
            .unwrap()
            .replace(SESSION, "00000000-0000-4000-8000-000000000002")
            .into_bytes();
        let padded = |bytes: &[u8]| {
            let mut bytes = bytes.to_vec();
            bytes.resize(original.len(), b' ');
            bytes
        };
        let truncated = original[..original.len() / 2].to_vec();
        for (case, changed) in [
            ("malformed rewrite", malformed),
            ("replaced session", replaced),
            ("null rewrite", padded(b"null")),
            ("array rewrite", padded(b"[]")),
            ("scalar rewrite", padded(b"1")),
            ("truncated during read", truncated),
        ] {
            std::fs::write(fixture.path(), &original).unwrap();
            let mut history = indexed(fixture.path());
            let mut startup = Claude {
                history: Some(indexed(fixture.path())),
                ..Claude::default()
            };
            let offset = history.offset;
            std::fs::write(fixture.path(), changed).unwrap();
            let failure = history.page_for_start().unwrap_err();
            assert!(
                matches!(failure, super::super::SetupError::Unavailable(_)),
                "{case}: {failure}"
            );
            assert_eq!(
                history.offset, offset,
                "a refused page must remain available after repair"
            );
            let failure = startup.initialize_for_start().unwrap_err();
            assert!(
                matches!(failure, super::super::SetupError::Unavailable(_)),
                "startup {case}: {failure}"
            );
            std::fs::write(fixture.path(), &original).unwrap();
            let page = history.page_for_start().unwrap();
            assert_eq!(page.len(), 1);
            assert_eq!(page[0]["sessionId"], SESSION);
            assert_eq!(page[0]["message"]["content"], "kept prompt");
            let mut repaired = Claude {
                history: Some(indexed(fixture.path())),
                ..Claude::default()
            };
            assert!(
                repaired.initialize_for_start().is_ok(),
                "startup repair: {case}"
            );
        }
    }

    fn padded_record(record: &Value) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(record).unwrap();
        assert!(bytes.len() < 1024);
        bytes.resize(1023, b' ');
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn indexed_tool_mutation_and_repair_keep_retryable_startup_errors() {
        let fixture = tempfile::NamedTempFile::new().unwrap();
        let call = json!({"type":"assistant", "uuid":"call", "parentUuid":null,
        "sessionId":SESSION, "message":{"id":"call-message", "role":"assistant", "content":[
            {"type":"tool_use", "id":"tool-read", "name":"Read", "input":{"file_path":"/fixture/file"}}
        ]}});
        let result = json!({"type":"user", "uuid":"result", "parentUuid":"call",
        "sessionId":SESSION, "message":{"role":"user", "content":[
            {"type":"tool_result", "tool_use_id":"tool-read", "content":"kept result"}
        ]}});
        let original = [padded_record(&call), padded_record(&result)].concat();
        for case in 0..8 {
            std::fs::write(fixture.path(), &original).unwrap();
            let mut history = indexed(fixture.path());
            let mut startup = Claude {
                history: Some(indexed(fixture.path())),
                ..Claude::default()
            };
            let offset = history.offset;
            let mut changed_call = call.clone();
            let mut changed_result = result.clone();
            match case {
                0 => changed_call["message"]["content"] = json!([]),
                1 => changed_call["message"]["content"][0]["type"] = json!("text"),
                2 => {
                    changed_call["message"]["content"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("name");
                }
                3 => changed_result["message"]["content"] = json!([]),
                4 => changed_result["message"]["content"][0]["type"] = json!("text"),
                5 => {
                    changed_result["message"]["content"][0]["tool_use_id"] = json!("different-tool")
                }
                6 => {
                    changed_result["message"]["content"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("tool_use_id");
                }
                7 => {
                    changed_call["message"]["content"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("id");
                }
                _ => unreachable!(),
            }
            std::fs::write(
                fixture.path(),
                [padded_record(&changed_call), padded_record(&changed_result)].concat(),
            )
            .unwrap();
            let failure = history.page_for_start().unwrap_err();
            assert!(
                matches!(failure, super::super::SetupError::Unavailable(_)),
                "case {case}: {failure}"
            );
            assert_eq!(history.offset, offset);
            let failure = startup.initialize_for_start().unwrap_err();
            assert!(
                matches!(failure, super::super::SetupError::Unavailable(_)),
                "startup case {case}: {failure}"
            );
            std::fs::write(fixture.path(), &original).unwrap();
            let page = history.page_for_start().unwrap();
            assert_eq!(page.len(), 2);
            assert_eq!(page[1].tool_calls[&0]["id"], "tool-read");
            assert_eq!(page[1]["message"]["content"][0]["tool_use_id"], "tool-read");
            let mut repaired = Claude {
                history: Some(indexed(fixture.path())),
                ..Claude::default()
            };
            assert!(
                repaired.initialize_for_start().is_ok(),
                "startup repair case {case}"
            );
        }
    }

    #[test]
    fn transcript_read_failure_is_retryable_but_invalid_history_is_not() {
        let session = "00000000-0000-4000-8000-000000000001";
        let mut fixture = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            fixture,
            "{}",
            json!({
                "type":"user", "uuid":"foreign-prompt", "parentUuid":null,
                "sessionId":"00000000-0000-4000-8000-000000000002",
                "message":{"role":"user","content":"foreign prompt"}
            })
        )
        .unwrap();
        assert!(matches!(
            History::index_for_start(File::open(fixture.path()).unwrap(), session, 1024, 16),
            Err(super::super::SetupError::Invalid(_))
        ));
        std::fs::write(
            fixture.path(),
            format!(
                "{}\n",
                json!({
                    "type":"user", "uuid":"prompt", "parentUuid":null, "sessionId":session,
                    "message":{"role":"user","content":"kept prompt"}
                })
            ),
        )
        .unwrap();
        // A regular file with an unusable read handle models the same OS read
        // failure as lost permissions/storage, without depending on test uid.
        let write_only = || {
            std::fs::OpenOptions::new()
                .write(true)
                .open(fixture.path())
                .unwrap()
        };
        assert!(matches!(
            History::index_for_start(write_only(), session, 1024, 16),
            Err(super::super::SetupError::Unavailable(_))
        ));
        let mut history = History::index(File::open(fixture.path()).unwrap(), session).unwrap();
        history.file = write_only();
        let mut adapter = Claude {
            history: Some(history),
            ..Claude::default()
        };
        assert!(matches!(
            adapter.initialize_for_start(),
            Err(super::super::SetupError::Unavailable(_))
        ));
        // Repairing the read handle admits exactly the same saved history.
        let history = History::index(File::open(fixture.path()).unwrap(), session).unwrap();
        let mut adapter = Claude {
            history: Some(history),
            ..Claude::default()
        };
        assert!(adapter.initialize_for_start().is_ok());
    }
}

#[cfg(test)]
mod fresh_recipe_constructor_tests {
    use super::*;

    const SAVED: &str = "00000000-0000-4000-8000-000000000001";
    const FOREIGN: &str = "00000000-0000-4000-8000-000000000002";

    fn source(root: &Path, arguments: Vec<String>) -> chat::Session {
        let home = root.join("home");
        let cwd = root.join("project");
        let config = home.join(".claude");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        chat::Session {
            id: crate::model::TerminalId::new(),
            launch: Launch {
                provider: Provider::Claude,
                session_id: Some(SAVED.into()),
                recipe: crate::model::LaunchRecipe {
                    program: root.join("claude").to_string_lossy().into_owned(),
                    arguments,
                    working_directory: cwd,
                    environment: std::collections::BTreeMap::from([
                        ("HOME".into(), home.to_string_lossy().into_owned()),
                        (
                            "CLAUDE_CONFIG_DIR".into(),
                            config.to_string_lossy().into_owned(),
                        ),
                    ]),
                    unset_environment: ["ANTHROPIC_API_KEY".into()].into(),
                },
            },
            proof: Some(crate::model::HostSessionProof {
                host_session_id: "old-host".into(),
                boot_id: "old-boot".into(),
                host_incarnation: "old-incarnation".into(),
            }),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        }
    }

    #[test]
    fn fresh_recipe_constructor_admits_claude_selectors_without_reusing_identity_or_payload_flags()
    {
        let root = tempfile::tempdir().unwrap();
        for (model, system, tool) in [
            ("--resume", "resume", "--resume"),
            ("resume", "--resume", "resume"),
        ] {
            let payload = vec![
                "--model".to_owned(),
                model.to_owned(),
                "--system-prompt".to_owned(),
                system.to_owned(),
                format!("--tools={tool}"),
            ];
            let selectors = [
                vec!["--resume".into(), SAVED.into()],
                vec![format!("--resume={SAVED}")],
                vec!["-r".into(), SAVED.into()],
                vec![format!("-r={SAVED}")],
                vec!["--session-id".into(), SAVED.into()],
                vec![format!("--session-id={SAVED}")],
                vec![
                    "--resume".into(),
                    SAVED.into(),
                    format!("-r={SAVED}"),
                    "--session-id".into(),
                    SAVED.into(),
                ],
            ];
            for selector in selectors {
                let saved = source(root.path(), [payload.clone(), selector.clone()].concat());
                let original = saved.clone();
                let fresh = saved
                    .fresh_start()
                    .expect("an exact selector can be removed for Fresh");
                assert_eq!(saved, original, "Fresh must leave the saved card untouched");
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
                let (spec, mut adapter) = create_for_start(&fresh.launch).unwrap();
                assert!(
                    spec.arguments
                        .windows(2)
                        .any(|pair| pair == ["--model", model])
                );
                assert!(
                    spec.arguments
                        .windows(2)
                        .any(|pair| pair == ["--system-prompt", system])
                );
                assert!(
                    spec.arguments
                        .windows(2)
                        .any(|pair| pair == ["--tools", tool])
                );
                let generated: Vec<_> = spec
                    .arguments
                    .iter()
                    .filter_map(|arg| arg.strip_prefix("--session-id="))
                    .collect();
                assert_eq!(generated.len(), 1);
                assert_ne!(generated[0], SAVED);
                assert!(uuid::Uuid::parse_str(generated[0]).is_ok());
                assert!(!spec.arguments.iter().any(|arg| arg == SAVED));
                let initial = adapter.initialize_for_start().unwrap();
                assert!(
                    !serde_json::to_string(&initial.writes)
                        .unwrap()
                        .contains(SAVED)
                );
                assert_eq!(saved, original);
                assert!(
                    !root.path().join("claude").exists(),
                    "constructors must not execute a provider"
                );
            }
        }
    }

    #[test]
    fn fresh_recipe_constructor_rejects_foreign_or_malformed_claude_selectors_without_mutating_source()
     {
        let root = tempfile::tempdir().unwrap();
        for arguments in [
            vec!["--resume".into()],
            vec!["--resume=".into()],
            vec!["-r=".into()],
            vec!["--session-id".into()],
            vec!["--resume".into(), FOREIGN.into()],
            vec![format!("--session-id={FOREIGN}")],
            vec!["--resume".into(), "latest".into()],
            vec!["--resume".into(), SAVED.into(), format!("-r={FOREIGN}")],
        ] {
            let saved = source(root.path(), arguments.clone());
            let original = saved.clone();
            assert!(saved.fresh_start().is_none(), "{arguments:?}");
            assert_eq!(saved, original);
        }
    }
}
